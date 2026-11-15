//! Turns Pulse's cluster feed into LiveKit connection strings.
//!
//! Per inbound `peer.*.cluster_change`, on one unqueued subscription (`serve` runs one island
//! subscriber, in the primary): extract the wallet from the subject, decode and validate its
//! session, serialize per wallet, run the platform access gate, evict any session
//! this event displaces, mint a token for the cluster's island room, hand the legacy
//! `IslandChangedMessage` the WS Connector forwards to its sockets by call ([`IslandSockets`]),
//! and record the assignment so the next event can name the room the peer came from.
//!
//! Per session the connector announces ([`ClusterSubscriber::announce_connect`], a call), query
//! Pulse's current wallet/session assignment and revalidate after minting. Local assignment hints
//! cannot establish current ownership after a gap.
//!
//! Idle without a configured link (`NATS_URL`): it subscribes to nothing, ignores announcements
//! and is indistinguishable from the component not existing.

use crate::gatekeeper::admission::cluster::safety::DispatchState;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use prost::Message as _;
use tokio::sync::Notify;

use crate::protocol::archipelago::archipelago::IslandChangedMessage;
use crate::protocol::bus::cluster::{ClusterChange, ClusterLookup, valid_reply};
use crate::protocol::bus::{BusError, MAX_PAYLOAD_BYTES, MAX_SUBJECT_BYTES};
use crate::protocol::pulse::decentraland::pulse::PeerClusterChange;
use crate::protocol::pulse::decentraland::pulse::PeerClusterSnapshot;

use crate::gatekeeper::admission::cluster::fence::{
    AssignmentDocument, AssignmentFence, PulseAdmissionStamp, TokenFence,
};
use crate::gatekeeper::admission::cluster::peer_state::{ClusterPeerState, PeerAssignment};
use crate::gatekeeper::admission::cluster::sockets::{IslandDelivery, IslandSockets};
use crate::gatekeeper::admission::cluster::{ClusterConfig, MAX_ISLAND_TOKEN_TTL_SECONDS};
use crate::gatekeeper::bus::{NatsBus, NatsHandler, NatsSubscription};
use crate::gatekeeper::livekit::Removal;
use crate::rooms::island_room_name;

const RECOVERY_DEADLINE: Duration = Duration::from_secs(10);
#[cfg(test)]
use super::safety::LIVEKIT_TOKEN_LEEWAY;
use super::safety::TOKEN_EXPIRY_SAFETY;

mod recovery;
#[cfg(test)]
use recovery::{CleanupReceipts, CleanupState};
use recovery::{RecoveryOwners, RecoveryTicket};
mod takeover;
use takeover::{TakeoverOwners, TakeoverTicket};
pub mod wallet_queue;
pub use wallet_queue::WalletQueue;
use wallet_queue::{QueuedWork, WorkOwner};

#[derive(Clone, Copy, thiserror::Error)]
pub enum GatewayError {
    #[error("cluster gateway operation failed")]
    Unknown,
    #[error("cluster gateway operation failed")]
    DefiniteRejection,
    #[error("cluster gateway operation failed")]
    RoomMissing,
}

impl std::fmt::Debug for GatewayError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("GatewayError")
    }
}

impl GatewayError {
    pub fn new(_private_detail: impl Into<String>) -> Self {
        Self::Unknown
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IslandOccupant {
    Absent,
    Session(String),
    Unattributed,
}

/// Everything the subscriber needs beyond the bus: the access gate and the three LiveKit
/// server-API calls the handover rests on.
///
/// Each call surfaces a plain result. The fail-open and fail-closed decisions are deliberately
/// left to the call sites below, because the two call sites make opposite ones for opposite
/// reasons.
#[async_trait]
pub trait ClusterGateway: Send + Sync {
    fn safety_store(&self) -> Option<super::safety::SafetyStore> {
        None
    }
    async fn ensure_recovery_room(&self, _room: &str) -> Result<(), GatewayError> {
        Err(GatewayError::Unknown)
    }
    async fn is_denied(&self, wallet: &str) -> Result<bool, GatewayError>;

    async fn mint_connection_string(
        &self,
        _admission: &crate::gatekeeper::admission::cluster::fence::PulseAdmissionStamp,
        wallet: &str,
        session: Option<&str>,
        room: &str,
        ttl_seconds: u64,
        not_before_unix: Option<u64>,
    ) -> Result<String, GatewayError>;

    async fn mint_fenced_connection_string(
        &self,
        _admission: &crate::gatekeeper::admission::cluster::fence::PulseAdmissionStamp,
        _wallet: &str,
        _room: &str,
        _ttl_seconds: u64,
        _fence: &TokenFence,
        _not_before_unix: Option<u64>,
    ) -> Result<String, GatewayError> {
        Err(GatewayError::new(
            "gateway does not support fenced island tokens",
        ))
    }

    async fn remove_participant(
        &self,
        room: &str,
        identity: &str,
        revoke_tokens_minted_before: Option<i64>,
    ) -> Result<Removal, GatewayError>;

    async fn holds_participant(&self, room: &str, identity: &str) -> Result<bool, GatewayError>;

    async fn island_occupant(
        &self,
        room: &str,
        wallet: &str,
    ) -> Result<IslandOccupant, GatewayError> {
        self.holds_participant(room, wallet).await.map(|held| {
            if held {
                IslandOccupant::Unattributed
            } else {
                IslandOccupant::Absent
            }
        })
    }
}

struct RecoveryGuard {
    ticket: RecoveryTicket,
    deadline: tokio::time::Instant,
    critical: AtomicBool,
}
impl RecoveryGuard {
    fn cancelled(&self) -> bool {
        self.ticket.is_cancelled() || tokio::time::Instant::now() >= self.deadline
    }
    fn enter(&self) -> CriticalCleanup<'_> {
        self.critical.store(true, Ordering::SeqCst);
        CriticalCleanup(self)
    }
}
struct CriticalCleanup<'a>(&'a RecoveryGuard);
impl Drop for CriticalCleanup<'_> {
    fn drop(&mut self) {
        self.0.critical.store(false, Ordering::SeqCst);
    }
}

pub struct ClusterSubscriber {
    #[cfg(test)]
    test_clock: Mutex<Option<(u64, tokio::time::Instant)>>,
    bus: Arc<dyn NatsBus>,
    gateway: Arc<dyn ClusterGateway>,
    peers: Arc<ClusterPeerState>,
    config: ClusterConfig,
    queue: Arc<WalletQueue>,
    lifecycle: Mutex<Lifecycle>,
    in_flight: AtomicUsize,
    idle: Notify,
    recovery: Arc<RecoveryOwners>,
    takeovers: Arc<TakeoverOwners>,
    #[cfg(test)]
    receipts: CleanupReceipts,
    safety: Option<super::safety::SafetyStore>,
    assignment_fence: Option<Arc<dyn AssignmentFence>>,
    /// The connector's sockets ([`Self::attach_sockets`]); minted assignments go nowhere without.
    sockets: Mutex<Option<Arc<dyn IslandSockets>>>,
    /// Set once the missing sockets have been reported, so the warning is logged once.
    unattached_reported: AtomicBool,
}

#[derive(Default)]
struct Lifecycle {
    subscription: Option<NatsSubscription>,
    owner: Option<Arc<WorkOwner>>,
}

/// The lower-cased ephemeral address shape Pulse publishes as `session` and the WS Connector
/// registers a socket under. A value that is not one never addresses a socket or a lookup.
pub fn is_session_key(value: &str) -> bool {
    value.len() == 42
        && value.starts_with("0x")
        && value.as_bytes()[2..]
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

/// Reject once at whichever entry boundary first sees an invalid assignment.
fn accept_assignment_session(wallet: &str, session: &str) -> bool {
    if is_session_key(session) {
        return true;
    }
    crate::gatekeeper::admission::cluster::metrics::cluster_island_undelivered();
    tracing::warn!(%wallet, "dropping cluster assignment with an invalid session");
    false
}

/// The start of the second after the current one, as a revocation boundary.
///
/// LiveKit Cloud revokes tokens whose `nbf` predates the boundary at second granularity, and the minter
/// stamps `nbf` with the mint second, so "now" would spare a token minted in this same second. The
/// next whole second is the boundary every already-minted token is before, and the replacement is
/// minted with its `nbf` set to it: LiveKit's `nbf` leeway leaves it usable at once.
/// Self-hosted LiveKit ignores the cutoff; this value does not protect against old-token reuse there.
#[cfg(test)]
fn next_whole_second_unix() -> u64 {
    now_unix_ms().div_euclid(1000) as u64 + 1
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn now_unix_seconds() -> u64 {
    now_unix_ms().div_euclid(1000).max(0) as u64
}

pub(crate) fn island_token_not_before(floor: u64, now_ms: u64) -> u64 {
    floor.max(now_ms.saturating_sub(1000) / 1000)
}

pub(crate) fn floor_wait_ms(floor: u64, now_ms: u64) -> Option<u64> {
    if floor > crate::protocol::bus::cluster::MAX_CONSUMER_SECONDS {
        return None;
    }
    let wait = floor
        .checked_mul(1000)?
        .checked_add(1000)?
        .saturating_sub(now_ms);
    (wait < 60_000).then_some(wait)
}

fn fenced_token_expires_at_unix(ttl_seconds: u64) -> u64 {
    now_unix_seconds().saturating_add(ttl_seconds)
}

fn fenced_token_is_fresh(expires_at_unix: Option<u64>) -> bool {
    expires_at_unix.is_some_and(|expires_at| {
        expires_at > now_unix_seconds().saturating_add(TOKEN_EXPIRY_SAFETY.as_secs())
    })
}

impl ClusterSubscriber {
    pub(crate) fn admission_now_ms(&self) -> u64 {
        #[cfg(test)]
        if let Some((base, elapsed)) = *self.test_clock.lock().unwrap() {
            return base.saturating_add(elapsed.elapsed().as_millis().min(u64::MAX as u128) as u64);
        }
        now_unix_ms().max(0) as u64
    }

    pub fn new(
        bus: Arc<dyn NatsBus>,
        gateway: Arc<dyn ClusterGateway>,
        peers: Arc<ClusterPeerState>,
        config: ClusterConfig,
    ) -> Arc<Self> {
        Self::new_with_assignment_fence(bus, gateway, peers, config, None)
    }

    pub fn new_with_assignment_fence(
        bus: Arc<dyn NatsBus>,
        gateway: Arc<dyn ClusterGateway>,
        peers: Arc<ClusterPeerState>,
        mut config: ClusterConfig,
        assignment_fence: Option<Arc<dyn AssignmentFence>>,
    ) -> Arc<Self> {
        config.island_token_ttl_seconds = config
            .island_token_ttl_seconds
            .min(MAX_ISLAND_TOKEN_TTL_SECONDS);
        let safety = gateway.safety_store();
        Arc::new(Self {
            #[cfg(test)]
            test_clock: Mutex::new(None),
            bus,
            gateway,
            peers,
            config,
            queue: Arc::new(WalletQueue::default()),
            lifecycle: Mutex::new(Lifecycle::default()),
            in_flight: AtomicUsize::new(0),
            idle: Notify::new(),
            recovery: Arc::new(RecoveryOwners::default()),
            takeovers: Arc::new(TakeoverOwners::default()),
            #[cfg(test)]
            receipts: CleanupReceipts::default(),
            safety,
            assignment_fence,
            sockets: Mutex::new(None),
            unattached_reported: AtomicBool::new(false),
        })
    }

    /// Hands every assignment minted from now on to `sockets`, the `/ws` connector's, in place
    /// of any attached before.
    pub fn attach_sockets(&self, sockets: Arc<dyn IslandSockets>) {
        *self.sockets.lock().unwrap() = Some(sockets);
    }

    pub fn peers(&self) -> &Arc<ClusterPeerState> {
        &self.peers
    }

    pub fn is_ready(&self) -> bool {
        self.bus.is_enabled() && self.bus.is_ready()
    }

    /// Registers the one subscription on Pulse's cluster feed and kicks off the connection; from
    /// then on [`Self::announce_connect`] takes announcements too.
    ///
    /// Unqueued: `serve` runs one island subscriber, in the primary, so each change reaches it
    /// once and is minted once.
    pub fn start(self: &Arc<Self>) {
        if !self.bus.is_enabled() {
            tracing::info!(
                "cluster subscriber staying idle: the NATS link is not configured (NATS_URL is unset)"
            );
            return;
        }

        let mut lifecycle = self.lifecycle.lock().unwrap();
        if lifecycle.owner.is_some() {
            return;
        }
        let owner = Arc::new(WorkOwner::default());
        lifecycle.owner = Some(owner.clone());
        lifecycle.subscription = Some(self.bus.subscribe(
            ClusterChange::SUBJECTS,
            None,
            self.guarded(owner, |me, owner, change| {
                me.handle_cluster_change(owner, change)
            }),
        ));
        drop(lifecycle);

        self.bus.connect();
        tracing::info!("cluster subscriber started");
    }

    /// Stops taking events, then drains whatever is mid-flight.
    ///
    /// In that order on purpose: an event arriving during the drain is not minted against a
    /// connection that is about to close; the wallet's next connect recovers it from Pulse.
    ///
    /// The graceful drain is bounded by `CLUSTER_DRAIN_TIMEOUT_MS`; 0 cancels immediately.
    /// After that grace, pending work is discarded and running futures are cancelled and joined.
    /// Only this incarnation is drained/cancelled, even when start races the old stop.
    /// Already submitted external side effects cannot be undone by cancelling their waiter.
    ///
    /// A bounded grace is required because
    /// this service has no stop timeout of its own, and a mint parked on a LiveKit request or a
    /// lookup parked on a broker that is gone would otherwise hold shutdown open until the
    /// orchestrator kills the process.
    pub async fn stop(&self) {
        let owner = {
            let mut lifecycle = self.lifecycle.lock().unwrap();
            let owner = lifecycle.owner.take();
            lifecycle.subscription = None;
            self.recovery.clear();
            owner
        };
        let Some(owner) = owner else { return };
        let budget = self.config.drain_timeout_ms;
        if budget > 0
            && tokio::time::timeout(Duration::from_millis(budget), owner.settle())
                .await
                .is_err()
        {
            tracing::warn!(
                in_flight = self.in_flight.load(Ordering::SeqCst),
                drain_timeout_ms = budget,
                "gave up draining the cluster subscriber"
            );
        }
        self.queue.cancel(&owner);
        owner.settle().await;
        tracing::info!("cluster subscriber stopped taking events");
    }

    /// Resolves once nothing is in flight.
    pub async fn settle(&self) {
        loop {
            let idle = self.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if self.in_flight.load(Ordering::SeqCst) == 0 {
                return;
            }
            idle.await;
        }
    }

    /// Wraps the cluster feed's callback so each change is decoded once and nothing escapes into
    /// the delivery loop: a panic there would stop delivery on every subject sharing the
    /// connection, not just this one.
    fn guarded(
        self: &Arc<Self>,
        owner: Arc<WorkOwner>,
        handle: fn(Arc<Self>, Arc<WorkOwner>, ClusterChange),
    ) -> NatsHandler {
        let weak = Arc::downgrade(self);
        Arc::new(move |subject: &str, data: &[u8]| {
            if subject.len() > MAX_SUBJECT_BYTES || data.len() > MAX_PAYLOAD_BYTES {
                crate::gatekeeper::admission::cluster::metrics::cluster_feed_oversized();
                return;
            }
            let Some(me) = Weak::upgrade(&weak) else {
                return;
            };
            let lifecycle = me.lifecycle.lock().unwrap();
            if !lifecycle
                .owner
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &owner))
            {
                return;
            }
            let message = match ClusterChange::decode(subject, data) {
                Ok(message) => message,
                Err(BusError::Subject) => {
                    crate::gatekeeper::admission::cluster::metrics::cluster_feed_invalid_subject();
                    tracing::warn!("feed subject does not contain a wallet address");
                    return;
                }
                Err(BusError::TooLarge) => {
                    crate::gatekeeper::admission::cluster::metrics::cluster_feed_oversized();
                    return;
                }
                Err(BusError::Payload(error)) => {
                    let wallet = ClusterChange::subject_wallet(subject).unwrap_or_default();
                    tracing::error!(%wallet, %error, "cannot decode a feed message");
                    return;
                }
            };
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handle(me.clone(), owner.clone(), message);
            }))
            .is_err()
            {
                tracing::error!("cluster feed callback panicked");
            }
        })
    }

    #[cfg(test)]
    fn spawn<F>(self: &Arc<Self>, wallet: String, work: F)
    where
        F: FnOnce(Arc<Self>) -> futures::future::BoxFuture<'static, ()> + Send + 'static,
    {
        let lifecycle = self.lifecycle.lock().unwrap();
        self.spawn_scoped(lifecycle.owner.as_ref().unwrap().clone(), wallet, work);
    }

    /// Queues one message's work, taking its place in the wallet's lane before returning, so the
    /// order this is called in is the order the work runs in.
    fn spawn_scoped<F>(self: &Arc<Self>, owner: Arc<WorkOwner>, wallet: String, work: F) -> bool
    where
        F: FnOnce(Arc<Self>) -> futures::future::BoxFuture<'static, ()> + Send + 'static,
    {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        let me = self.clone();
        let queued: QueuedWork = Box::pin(async move { work(me.clone()).await });
        let settled = self.clone();
        self.queue.enqueue_scoped(
            owner,
            &wallet,
            queued,
            Box::new(move || {
                settled.in_flight.fetch_sub(1, Ordering::SeqCst);
                settled.idle.notify_waiters();
            }),
        )
    }

    /// Validates a decoded event before invalidating recovery or queuing its work.
    fn handle_cluster_change(self: Arc<Self>, owner: Arc<WorkOwner>, message: ClusterChange) {
        let ClusterChange { wallet, change } = message;
        crate::gatekeeper::admission::cluster::metrics::cluster_event_received();

        if change.cluster_id.is_empty() {
            tracing::warn!(%wallet, "cannot process a cluster_change with an empty cluster_id");
            return;
        }

        if !accept_assignment_session(&wallet, &change.session) {
            return;
        }

        self.recovery.invalidate(&wallet);

        let takeover = (self.config.self_hosted_token_quarantine
            && ((!change.displaced_session.is_empty()
                && change.displaced_cluster_id == change.cluster_id)
                || change
                    .pending_takeovers
                    .iter()
                    .any(|op| op.displaced_cluster_id == change.cluster_id)))
        .then(|| {
            self.takeovers
                .prepare(&wallet, &change.cluster_id, &change.session, &change.realm)
        });
        let (registration, ticket) = takeover
            .map(|(registration, ticket)| (Some(registration), Some(ticket)))
            .unwrap_or_default();
        let for_wallet = wallet.clone();
        let accepted = self.spawn_scoped(owner, wallet, move |me| {
            Box::pin(async move {
                me.process_cluster_change_inner(&for_wallet, &change, ticket)
                    .await
            })
        });
        if accepted && let Some(registration) = registration {
            registration.commit();
        }
    }

    /// Takes the connector's announcement that `session` of `wallet` holds a live socket (on the
    /// welcome, a v4 realm claim, and every re-announce round) and queues its recovery on the same
    /// lane as cluster changes, so within one process a reconnect cannot interleave with a move
    /// for the same wallet. A stopped or never-started subscriber ignores it.
    ///
    /// Both are lower-cased; the shape is strict, including whitespace. Repeated announcements for
    /// a currently recovering session are coalesced, so a slow mint is not starved by periodic
    /// retries.
    pub fn announce_connect(self: &Arc<Self>, wallet: &str, session: &str) {
        let lifecycle = self.lifecycle.lock().unwrap();
        let Some(owner) = lifecycle.owner.clone() else {
            return;
        };
        crate::gatekeeper::admission::cluster::metrics::cluster_connect_received();
        let wallet = wallet.to_ascii_lowercase();
        let session = session.to_ascii_lowercase();
        if !is_session_key(&wallet) || !is_session_key(&session) {
            crate::gatekeeper::admission::cluster::metrics::cluster_reannounce_unresolved();
            return;
        }
        let Some(ticket) = self.recovery.begin(&wallet, &session) else {
            return;
        };
        let for_wallet = wallet.clone();
        self.spawn_scoped(owner, wallet, move |me| {
            Box::pin(async move { me.recover_guarded(&for_wallet, &session, ticket).await })
        });
    }

    /// The one gate in this service that fails open: a background feed has no caller to return an
    /// error to, and failing closed would stop island formation for everyone during an outage of
    /// the store behind it. Nothing is remembered on failure, so the next event retries.
    ///
    /// Only this platform's ban store is consulted. Upstream also ORs in a deny-list lookup, for
    /// which this platform has no equivalent store; see the stream notes.
    async fn is_denied(&self, wallet: &str) -> bool {
        match self.gateway.is_denied(wallet).await {
            Ok(denied) => denied,
            Err(_) => {
                crate::gatekeeper::admission::cluster::metrics::cluster_access_check_failed();
                tracing::warn!(%wallet, reason = "gateway failure", "access check failed; allowing");
                false
            }
        }
    }

    /// Gates, evicts, mints and delivers one assignment.
    ///
    /// A repeat assignment is not suppressed: Pulse re-announces a cluster only after forgetting a
    /// peer, which is a reconnect that needs a fresh token.
    ///
    /// Only a valid session key may mint or receive an assignment; malformed or empty
    /// session keys are dropped and counted before any access check or side effect.
    ///
    /// Only an assignment handed to the connector's sockets updates the previous-room hint. Even
    /// that is not a client receipt or a successful join: current-state reconciliation is still
    /// needed.
    pub async fn process_cluster_change(&self, wallet: &str, change: &PeerClusterChange) {
        self.process_cluster_change_inner(wallet, change, None)
            .await;
    }

    async fn process_cluster_change_inner(
        &self,
        wallet: &str,
        change: &PeerClusterChange,
        takeover: Option<TakeoverTicket>,
    ) {
        if change.cluster_id.is_empty() || !accept_assignment_session(wallet, &change.session) {
            return;
        }
        self.reconcile_current(wallet, &change.session, true, takeover.as_ref())
            .await;
    }

    /// Serializes the authority row from the live-source lookup through token mint and publish.
    /// This fences the control owner and assignment publication, not LiveKit admission: LiveKit
    /// does not interpret the token fence and an already issued JWT remains usable until expiry.
    /// Consequently, owner replacements that would require destructive SFU work fail closed.
    ///
    /// Returns false for work the authority does not cover, which the caller takes down the
    /// legacy path: a wallet no v4 socket has claimed, or a session that cannot be a v4 owner.
    /// The same server admits unmodified v3 clients, and they write no authority row.
    async fn process_fenced_cluster_change(
        &self,
        wallet: &str,
        change: &PeerClusterChange,
    ) -> bool {
        if !is_session_key(wallet) || !is_session_key(&change.session) {
            return false;
        }
        let authority = self
            .assignment_fence
            .as_ref()
            .expect("fenced path requires authority");
        let lease = match authority.lock_realm(wallet, &change.session).await {
            Ok(lease) => lease,
            Err(crate::gatekeeper::admission::cluster::fence::FenceError::Unowned) => return false,
            Err(error) => {
                crate::gatekeeper::admission::cluster::metrics::cluster_recovery_stale();
                tracing::warn!(%wallet, %error, "realm assignment authority rejected cluster work");
                return true;
            }
        };
        let Some(snapshot) = self.lookup_current(wallet, &change.session).await else {
            crate::gatekeeper::admission::cluster::metrics::cluster_reannounce_unresolved();
            return true;
        };
        if snapshot.cluster_id != change.cluster_id || snapshot.realm != change.realm {
            crate::gatekeeper::admission::cluster::metrics::cluster_recovery_stale();
            return true;
        }
        if !snapshot.pending_takeovers.is_empty() || snapshot.token_not_before.is_none() {
            crate::gatekeeper::admission::cluster::metrics::cluster_takeover_failed();
            tracing::warn!(
                %wallet,
                displaced_session = %change.displaced_session,
                "fenced takeover withheld: LiveKit removal is not session-conditional"
            );
            return true;
        }

        let Some(snapshot) = self.ready_floor(&snapshot).await else {
            return true;
        };
        let room = island_room_name(&snapshot.cluster_id);
        let fence = lease.token_fence();
        let token_expires_at_unix =
            fenced_token_expires_at_unix(self.config.island_token_ttl_seconds);
        let Some(admission) = self.credential_stamp(&snapshot).await else {
            return true;
        };
        let not_before = admission.credential_not_before;
        let conn_str = match self
            .gateway
            .mint_fenced_connection_string(
                &admission,
                wallet,
                &room,
                self.config.island_token_ttl_seconds,
                &fence,
                Some(not_before),
            )
            .await
        {
            Ok(conn_str) => conn_str,
            Err(_) => {
                tracing::error!(%wallet, %room, "cannot mint a fenced island token");
                return true;
            }
        };
        crate::gatekeeper::admission::cluster::metrics::cluster_token_minted();
        let Some(current) = self.lookup_current(wallet, &change.session).await else {
            crate::gatekeeper::admission::cluster::metrics::cluster_recovery_stale();
            return true;
        };
        if !snapshot_extends(&snapshot, &current)
            || current.token_not_before != snapshot.token_not_before
            || !current.pending_takeovers.is_empty()
            || !self.stamp_ready(&admission).await
            || floor_wait_ms(not_before, self.admission_now_ms()) != Some(0)
        {
            crate::gatekeeper::admission::cluster::metrics::cluster_recovery_stale();
            return true;
        }
        let assignment = AssignmentDocument {
            pulse_admission: Some(admission.clone()),
            island_id: room,
            connection_string: conn_str,
            from_island_id: lease.previous_island(),
            token_expires_at_unix: Some(token_expires_at_unix),
            peers: Default::default(),
        };
        if let Err(error) = lease.commit(assignment).await {
            crate::gatekeeper::admission::cluster::metrics::cluster_publish_failed();
            tracing::warn!(%wallet, %error, "fenced realm assignment commit rejected");
            return true;
        }
        crate::gatekeeper::admission::cluster::metrics::cluster_published();
        tracing::info!(
            %wallet,
            source_incarnation = %snapshot.source_incarnation,
            source_pass = snapshot.pass,
            owner_epoch = fence.owner_epoch,
            assignment_revision = fence.assignment_revision,
            fencing_token = fence.fencing_token,
            "fenced realm assignment committed"
        );
        true
    }

    /// Hands the assignment to the connector's socket of this validated session
    /// and records it as the room the peer was last told to
    /// join, whether or not a socket here took it. Only a delivery counts as published; an
    /// assignment no socket took counts as undelivered (the sockets of another `serve` are out of
    /// reach). With no sockets attached it reaches nobody and records nothing.
    fn deliver_assignment(
        &self,
        wallet: &str,
        cluster_id: &str,
        session: &str,
        conn_str: String,
        stamp: &PulseAdmissionStamp,
    ) {
        let sockets = self.sockets.lock().unwrap().clone();
        let Some(sockets) = sockets else {
            crate::gatekeeper::admission::cluster::metrics::cluster_island_undelivered();
            if !self.unattached_reported.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    %wallet,
                    "no connector sockets are attached: island assignments reach no client"
                );
            }
            return;
        };
        let room = island_room_name(cluster_id);
        let message = IslandChangedMessage {
            island_id: room.clone(),
            conn_str,
            from_island_id: self.peers.assignment(wallet).map(|previous| previous.room),
            peers: HashMap::new(),
            assignment_context: None,
        };
        match sockets.deliver_island(wallet, session, &message, Some(stamp)) {
            IslandDelivery::Delivered => {
                crate::gatekeeper::admission::cluster::metrics::cluster_published()
            }
            IslandDelivery::Undelivered => {
                crate::gatekeeper::admission::cluster::metrics::cluster_island_undelivered()
            }
        }
        self.peers.record_assignment(
            wallet,
            PeerAssignment {
                cluster_id: cluster_id.into(),
                room,
                last_seen_ms: now_unix_ms(),
            },
        );
    }

    /// Reconcile only an exact session, never a legacy or cached guess. A snapshot is not a lease:
    /// source restart, revision rollback or changed assignment during minting discards the candidate.
    /// Local change events cancel the whole attempt; another socket's connect is not ownership
    /// evidence and cannot cancel it. Legacy recovery is suppressed by a matching or unattributed
    /// SFU participant. Fenced recovery refills a cleared claim even when an old matching participant
    /// remains, but suppresses a healthy matching participant once the authority already holds the
    /// room. Other occupants rely on fenced LiveKit admission, never a blind RemoveParticipant.
    pub async fn process_peer_connect(&self, wallet: &str, session: &str) {
        if !is_session_key(wallet) || !is_session_key(session) {
            return;
        }
        let Some(ticket) = self.recovery.begin(wallet, session) else {
            return;
        };
        self.recover_guarded(wallet, session, ticket).await;
    }

    async fn recover_guarded(&self, wallet: &str, session: &str, ticket: RecoveryTicket) {
        let guard = RecoveryGuard {
            ticket,
            deadline: tokio::time::Instant::now() + RECOVERY_DEADLINE,
            critical: AtomicBool::new(false),
        };
        let recovery = self.reconcile_with_guard(wallet, session, false, None, Some(&guard));
        tokio::pin!(recovery);
        tokio::select! {
            biased;
            _ = guard.ticket.cancelled() => crate::gatekeeper::admission::cluster::metrics::cluster_recovery_stale(),
            _ = tokio::time::sleep_until(guard.deadline) => crate::gatekeeper::admission::cluster::metrics::cluster_reannounce_unresolved(),
            _ = &mut recovery => return,
        }
        // Keep the original future, wallet lane and WorkOwner charge through
        // exact settlement. Queue hard stop and 150s expiry remain outside.
        if guard.critical.load(Ordering::SeqCst) {
            recovery.await;
        }
    }

    async fn lookup_current(&self, wallet: &str, session: &str) -> Option<PeerClusterSnapshot> {
        if !is_session_key(wallet) || !is_session_key(session) {
            return None;
        }
        let lookup = ClusterLookup {
            wallet: wallet.to_owned(),
            session: session.to_owned(),
        };
        let payload = self
            .bus
            .request(&lookup.subject(), lookup.encode())
            .await
            .ok()?;
        if payload.len() > crate::protocol::bus::cluster::MAX_REPLY_BYTES {
            return None;
        }
        let snapshot = PeerClusterSnapshot::decode(payload.as_slice()).ok()?;
        valid_reply(&lookup, &snapshot).then_some(snapshot)
    }

    #[cfg(test)]
    pub(crate) async fn verify_stamp(&self, stamp: &PulseAdmissionStamp) -> bool {
        if !stamp.valid() || self.is_denied(&stamp.wallet).await {
            return false;
        }
        let Some(current) = self.lookup_current(&stamp.wallet, &stamp.session).await else {
            return false;
        };
        stamp.matches_snapshot(&current) && self.stamp_ready(stamp).await
    }

    /// Final durable SQL/time check after the caller's last broker/policy wait.
    pub(crate) async fn stamp_ready(&self, stamp: &PulseAdmissionStamp) -> bool {
        let now = self.admission_now_ms();
        if !stamp.valid()
            || floor_wait_ms(stamp.token_not_before, now) != Some(0)
            || floor_wait_ms(stamp.credential_not_before, now) != Some(0)
        {
            return false;
        }
        if let Some(store) = &self.safety {
            let Ok(lease) = store
                .authorize_stamp(
                    stamp,
                    self.config.control_v4_audience.as_deref(),
                    None,
                    false,
                )
                .await
            else {
                return false;
            };
            return lease.commit().await.is_ok();
        }
        #[cfg(test)]
        {
            self.receipts.safe_to_mint(&stamp.wallet)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    async fn safety_ready(&self, wallet: &str) -> bool {
        if let Some(store) = &self.safety {
            return store.snapshot(wallet).await.is_ok();
        }
        #[cfg(test)]
        {
            self.receipts.safe_to_mint(wallet)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    async fn acknowledge_receipts(&self, snapshot: &PeerClusterSnapshot) -> bool {
        if let Some(store) = &self.safety {
            return store
                .acknowledge(
                    &snapshot.wallet,
                    &snapshot.source_incarnation,
                    &snapshot.pending_takeovers,
                )
                .await
                .is_ok();
        }
        #[cfg(test)]
        {
            self.receipts.acknowledge(snapshot);
            true
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    async fn credential_stamp(
        &self,
        snapshot: &PeerClusterSnapshot,
    ) -> Option<PulseAdmissionStamp> {
        let mut not_before =
            island_token_not_before(snapshot.token_not_before?, self.admission_now_ms());
        let safety = if let Some(store) = &self.safety {
            Some(store.snapshot(&snapshot.wallet).await.ok()?)
        } else {
            None
        };
        #[cfg(not(test))]
        safety?;
        if let Some(safety) = safety {
            not_before = not_before.max(safety.token_floor);
        }
        let wait = floor_wait_ms(not_before, self.admission_now_ms())?;
        if wait > 0 {
            tokio::time::sleep(Duration::from_millis(wait)).await;
            let current = self.same_authority(snapshot).await?;
            if current.token_not_before != snapshot.token_not_before
                || !current.pending_takeovers.is_empty()
            {
                return None;
            }
        }
        let mut stamp = PulseAdmissionStamp::from_snapshot(snapshot, not_before)?;
        if let Some(safety) = safety {
            stamp.safety_generation = safety.generation;
            stamp.safety_token_floor = safety.token_floor;
        }
        self.stamp_ready(&stamp).await.then_some(stamp)
    }

    async fn same_authority(&self, expected: &PeerClusterSnapshot) -> Option<PeerClusterSnapshot> {
        let current = self
            .lookup_current(&expected.wallet, &expected.session)
            .await?;
        snapshot_extends(expected, &current).then_some(current)
    }

    async fn ready_floor(&self, expected: &PeerClusterSnapshot) -> Option<PeerClusterSnapshot> {
        let current = self.same_authority(expected).await?;
        if !crate::protocol::bus::cluster::active_selection(&current) {
            return None;
        }
        if !self.safety_ready(&current.wallet).await {
            return None;
        }
        let floor = current.token_not_before?;
        if !current.pending_takeovers.is_empty() {
            return None;
        }
        let wait = floor_wait_ms(floor, self.admission_now_ms())?;
        if wait == 0 {
            return Some(current);
        }
        tokio::time::sleep(Duration::from_millis(wait)).await;
        let after = self.same_authority(&current).await?;
        (after.pending_takeovers.is_empty()
            && after.token_not_before == Some(floor)
            && floor_wait_ms(floor, self.admission_now_ms()) == Some(0)
            && self.safety_ready(&after.wallet).await)
            .then_some(after)
    }

    async fn operation_current(
        &self,
        expected: &PeerClusterSnapshot,
        operation: &crate::protocol::pulse::decentraland::pulse::PendingTakeover,
    ) -> bool {
        self.same_authority(expected)
            .await
            .is_some_and(|current| current.pending_takeovers.contains(operation))
    }

    async fn clean_retained(
        &self,
        expected: &PeerClusterSnapshot,
        takeover: Option<&TakeoverTicket>,
        guard: Option<&RecoveryGuard>,
    ) -> bool {
        if let Some(store) = &self.safety {
            return self.clean_durable(store, expected, guard).await;
        }
        #[cfg(test)]
        {
            return self
                .clean_process_local_for_unit_model(expected, takeover)
                .await;
        }
        #[cfg(not(test))]
        {
            let _ = takeover;
            false
        }
    }

    #[cfg(test)]
    async fn clean_process_local_for_unit_model(
        &self,
        expected: &PeerClusterSnapshot,
        takeover: Option<&TakeoverTicket>,
    ) -> bool {
        for operation in &expected.pending_takeovers {
            if !self.operation_current(expected, operation).await {
                return false;
            }
            let source = &expected.source_incarnation;
            let wallet = &expected.wallet;
            let room = island_room_name(&operation.displaced_cluster_id);
            let mut minimum = expected.token_not_before.unwrap_or(0);
            let mut created = false;
            let mut settled = None;
            for attempt in 0..3 {
                if !self.operation_current(expected, operation).await {
                    return false;
                }
                let Some((receipt, fresh)) = self.receipts.reserve(
                    source,
                    wallet,
                    operation,
                    minimum,
                    self.admission_now_ms() / 1000,
                    self.config.island_token_ttl_seconds,
                ) else {
                    return false;
                };
                if !fresh {
                    settled = Some(receipt);
                    break;
                }
                // The durable-for-this-process dispatch receipt already exists before polling RPC.
                let outcome = self
                    .gateway
                    .remove_participant(&room, wallet, Some(receipt.cutoff as i64))
                    .await;
                match outcome {
                    Ok(_) => {
                        if !self.config.self_hosted_token_quarantine
                            && self.admission_now_ms().saturating_add(1000)
                                >= receipt.cutoff.saturating_mul(1000)
                        {
                            // Positively settled, but too late to confirm this cutoff. A new
                            // checked cutoff is allowed; unknown outcomes never enter this branch.
                            self.receipts.reject(source, wallet, &operation.takeover_id);
                            minimum = receipt.cutoff;
                            if attempt == 2 {
                                return false;
                            }
                            continue;
                        }
                        let state = if self.config.self_hosted_token_quarantine
                            && operation.displaced_cluster_id == expected.cluster_id
                        {
                            CleanupState::Quarantine(
                                tokio::time::Instant::now()
                                    + Duration::from_secs(self.config.island_token_ttl_seconds)
                                        .saturating_add(LIVEKIT_TOKEN_LEEWAY)
                                        .saturating_add(TOKEN_EXPIRY_SAFETY),
                            )
                        } else {
                            CleanupState::Confirmed
                        };
                        self.receipts
                            .set(source, wallet, &operation.takeover_id, state.clone());
                        let mut confirmed = receipt;
                        confirmed.state = state;
                        settled = Some(confirmed);
                        break;
                    }
                    Err(GatewayError::RoomMissing) => {
                        self.receipts.reject(source, wallet, &operation.takeover_id);
                        minimum = receipt.cutoff;
                        if created || !self.operation_current(expected, operation).await {
                            return false;
                        }
                        created = true;
                        if self.gateway.ensure_recovery_room(&room).await.is_err()
                            || !self.operation_current(expected, operation).await
                        {
                            return false;
                        }
                    }
                    Err(GatewayError::DefiniteRejection) => {
                        self.receipts.reject(source, wallet, &operation.takeover_id);
                        minimum = receipt.cutoff;
                        if attempt < 2 && self.config.takeover_retry_delay_ms > 0 {
                            tokio::time::sleep(Duration::from_millis(
                                self.config.takeover_retry_delay_ms,
                            ))
                            .await;
                        }
                    }
                    Err(GatewayError::Unknown) => return false,
                }
            }
            let Some(receipt) = settled else { return false };
            let mut state = receipt.state;
            if !self.operation_current(expected, operation).await {
                return false;
            }
            if let CleanupState::Quarantine(until) = state {
                if let Some(takeover) = takeover {
                    loop {
                        tokio::select! {
                            _ = tokio::time::sleep_until(until) => break,
                            successor = takeover.superseded() => {
                                if self.lookup_current(wallet, &successor.session).await.is_some_and(|current| current.cluster_id == successor.cluster && current.realm == successor.realm && !snapshot_extends(expected, &current)) { return false; }
                            }
                        }
                    }
                } else {
                    tokio::time::sleep_until(until).await;
                }
                if !self.operation_current(expected, operation).await {
                    return false;
                }
                self.receipts.set(
                    source,
                    wallet,
                    &operation.takeover_id,
                    CleanupState::Dispatched,
                );
                match self
                    .gateway
                    .remove_participant(&room, wallet, Some(receipt.cutoff as i64))
                    .await
                {
                    Ok(_) => self.receipts.set(
                        source,
                        wallet,
                        &operation.takeover_id,
                        CleanupState::Confirmed,
                    ),
                    Err(GatewayError::DefiniteRejection | GatewayError::RoomMissing) => {
                        self.receipts.reject(source, wallet, &operation.takeover_id);
                        return false;
                    }
                    Err(GatewayError::Unknown) => return false,
                }
                state = CleanupState::Confirmed;
            }
            if state != CleanupState::Confirmed
                || !self.safety_ready(wallet).await
                || !self.operation_current(expected, operation).await
            {
                return false;
            }
            let completion = crate::protocol::bus::cluster::Completion {
                wallet: wallet.clone(),
                completion: crate::protocol::pulse::decentraland::pulse::TakeoverCompleted {
                    takeover_id: operation.takeover_id.clone(),
                    revoke_before: receipt.cutoff,
                },
            };
            let _ = self
                .bus
                .publish(&completion.subject(), completion.completion.encode_to_vec())
                .await;
            if !self
                .same_authority(expected)
                .await
                .is_some_and(|current| !current.pending_takeovers.contains(operation))
            {
                return false;
            }
        }
        true
    }

    async fn reconcile_current(
        &self,
        wallet: &str,
        session: &str,
        from_hint: bool,
        takeover: Option<&TakeoverTicket>,
    ) {
        self.reconcile_with_guard(wallet, session, from_hint, takeover, None)
            .await;
    }

    async fn reconcile_with_guard(
        &self,
        wallet: &str,
        session: &str,
        from_hint: bool,
        takeover: Option<&TakeoverTicket>,
        guard: Option<&RecoveryGuard>,
    ) {
        let Some(snapshot) = self.lookup_current(wallet, session).await else {
            crate::gatekeeper::admission::cluster::metrics::cluster_reannounce_unresolved();
            return;
        };
        if !self.acknowledge_receipts(&snapshot).await {
            return;
        }
        if self.is_denied(wallet).await {
            crate::gatekeeper::admission::cluster::metrics::cluster_banned_skipped();
            return;
        }
        if self.same_authority(&snapshot).await.is_none() {
            crate::gatekeeper::admission::cluster::metrics::cluster_recovery_stale();
            return;
        }
        if !snapshot.pending_takeovers.is_empty() {
            if snapshot.token_not_before.is_none() {
                return;
            }
            if !self.clean_retained(&snapshot, takeover, guard).await {
                return;
            }
        }
        let Some(ready) = self.ready_floor(&snapshot).await else {
            return;
        };
        if self.assignment_fence.is_some() {
            if from_hint {
                let hint = PeerClusterChange {
                    cluster_id: ready.cluster_id.clone(),
                    realm: ready.realm.clone(),
                    session: ready.session.clone(),
                    ..Default::default()
                };
                if self.process_fenced_cluster_change(wallet, &hint).await {
                    return;
                }
            } else if self.process_fenced_recovery(wallet, session, &ready).await {
                return;
            }
        }
        let room = island_room_name(&ready.cluster_id);
        if !from_hint && !self.recovery_room_available(wallet, session, &room).await {
            return;
        }
        let Some(before) = self.ready_floor(&ready).await else {
            return;
        };
        if !from_hint {
            crate::gatekeeper::admission::cluster::metrics::cluster_reannounce_attempted();
        }
        let Some(admission) = self.credential_stamp(&before).await else {
            return;
        };
        let not_before = admission.credential_not_before;
        let Ok(conn_str) = self
            .gateway
            .mint_connection_string(
                &admission,
                wallet,
                Some(session),
                &room,
                self.config.island_token_ttl_seconds,
                Some(not_before),
            )
            .await
        else {
            return;
        };
        crate::gatekeeper::admission::cluster::metrics::cluster_token_minted();
        if floor_wait_ms(not_before, self.admission_now_ms()) != Some(0) {
            return;
        }
        if !from_hint && !self.recovery_room_available(wallet, session, &room).await {
            return;
        }
        let Some(after) = self.ready_floor(&before).await else {
            return;
        };
        if !self.acknowledge_receipts(&after).await {
            return;
        }
        if after.token_not_before != before.token_not_before
            || floor_wait_ms(not_before, self.admission_now_ms()) != Some(0)
            || !self.stamp_ready(&admission).await
        {
            return;
        }
        if admission.matches_snapshot(&after) {
            self.deliver_assignment(wallet, &after.cluster_id, session, conn_str, &admission);
        }
    }

    /// Rebuilds an assignment for a socket that owns the shared v4 authority row. `Unowned` is
    /// the only result that returns false and permits legacy recovery. A current-room assignment is
    /// reused while its token is fresh, or while its exact-session participant remains healthy. An
    /// expired, unstamped assignment is renewed once the exact participant is gone. A newly claimed
    /// row has no assignment, so an old participant with the same session must not suppress refill.
    async fn process_fenced_recovery(
        &self,
        wallet: &str,
        session: &str,
        initial: &PeerClusterSnapshot,
    ) -> bool {
        let authority = self
            .assignment_fence
            .as_ref()
            .expect("fenced recovery requires authority");
        let lease = match authority.lock_realm(wallet, session).await {
            Ok(lease) => lease,
            Err(crate::gatekeeper::admission::cluster::fence::FenceError::Unowned) => return false,
            Err(error) => {
                crate::gatekeeper::admission::cluster::metrics::cluster_recovery_stale();
                tracing::warn!(%wallet, %error, "realm assignment authority rejected recovery");
                return true;
            }
        };
        let Some(snapshot) = self.lookup_current(wallet, session).await else {
            crate::gatekeeper::admission::cluster::metrics::cluster_reannounce_unresolved();
            return true;
        };
        if !snapshot_extends(initial, &snapshot) {
            crate::gatekeeper::admission::cluster::metrics::cluster_recovery_stale();
            return true;
        }
        let room = island_room_name(&snapshot.cluster_id);
        let previous_island = lease.previous_island();
        let previous_is_current = previous_island.as_deref() == Some(room.as_str());
        let previous_token_is_fresh = fenced_token_is_fresh(lease.previous_token_expires_at_unix());
        crate::gatekeeper::admission::cluster::metrics::cluster_reannounce_attempted();
        let Some(occupant) = self.fenced_recovery_room_occupant(wallet, &room).await else {
            return true;
        };
        let previous_stamp_is_current = if let Some(stamp) = lease.previous_pulse_admission() {
            stamp.matches_snapshot(&snapshot) && self.stamp_ready(&stamp).await
        } else {
            false
        };
        if previous_is_current
            && previous_stamp_is_current
            && (matches!(&occupant, IslandOccupant::Session(owner) if owner == session)
                || (previous_token_is_fresh
                    && matches!(
                        occupant,
                        IslandOccupant::Absent | IslandOccupant::Session(_)
                    )))
        {
            crate::gatekeeper::admission::cluster::metrics::cluster_reannounce_suppressed();
            return true;
        }
        let Some(snapshot) = self.ready_floor(&snapshot).await else {
            return true;
        };
        let fence = lease.token_fence();
        let token_expires_at_unix =
            fenced_token_expires_at_unix(self.config.island_token_ttl_seconds);
        let Some(admission) = self.credential_stamp(&snapshot).await else {
            return true;
        };
        let not_before = admission.credential_not_before;
        let conn_str = match self
            .gateway
            .mint_fenced_connection_string(
                &admission,
                wallet,
                &room,
                self.config.island_token_ttl_seconds,
                &fence,
                Some(not_before),
            )
            .await
        {
            Ok(conn_str) => conn_str,
            Err(_) => {
                tracing::error!(%wallet, %room, "cannot mint a fenced island token for recovery");
                return true;
            }
        };
        crate::gatekeeper::admission::cluster::metrics::cluster_token_minted();
        if self
            .fenced_recovery_room_occupant(wallet, &room)
            .await
            .is_none()
        {
            return true;
        }
        let Some(current) = self.lookup_current(wallet, session).await else {
            crate::gatekeeper::admission::cluster::metrics::cluster_recovery_stale();
            return true;
        };
        if !snapshot_extends(&snapshot, &current)
            || current.token_not_before != snapshot.token_not_before
            || !current.pending_takeovers.is_empty()
            || !self.stamp_ready(&admission).await
            || floor_wait_ms(not_before, self.admission_now_ms()) != Some(0)
        {
            crate::gatekeeper::admission::cluster::metrics::cluster_recovery_stale();
            return true;
        }
        let assignment = AssignmentDocument {
            pulse_admission: Some(admission.clone()),
            island_id: room,
            connection_string: conn_str,
            from_island_id: previous_island,
            token_expires_at_unix: Some(token_expires_at_unix),
            peers: Default::default(),
        };
        if let Err(error) = lease.commit(assignment).await {
            crate::gatekeeper::admission::cluster::metrics::cluster_publish_failed();
            tracing::warn!(%wallet, %error, "fenced realm recovery commit rejected");
            return true;
        }
        crate::gatekeeper::admission::cluster::metrics::cluster_published();
        tracing::info!(
            %wallet,
            source_incarnation = %current.source_incarnation,
            source_pass = current.pass,
            owner_epoch = fence.owner_epoch,
            assignment_revision = fence.assignment_revision,
            fencing_token = fence.fencing_token,
            "fenced realm assignment recovered"
        );
        true
    }

    async fn fenced_recovery_room_occupant(
        &self,
        wallet: &str,
        room: &str,
    ) -> Option<IslandOccupant> {
        match self.gateway.island_occupant(room, wallet).await {
            Ok(occupant @ (IslandOccupant::Absent | IslandOccupant::Session(_))) => Some(occupant),
            Ok(IslandOccupant::Unattributed) => {
                crate::gatekeeper::admission::cluster::metrics::cluster_reannounce_suppressed();
                None
            }
            Err(_) => {
                crate::gatekeeper::admission::cluster::metrics::cluster_reannounce_check_failed();
                tracing::warn!(
                    %wallet,
                    %room,
                    reason = "gateway failure",
                    "cannot safely recover a fenced island assignment"
                );
                None
            }
        }
    }

    async fn recovery_room_available(&self, wallet: &str, session: &str, room: &str) -> bool {
        let occupant = match self.gateway.island_occupant(room, wallet).await {
            Ok(occupant) => occupant,
            Err(_) => {
                crate::gatekeeper::admission::cluster::metrics::cluster_reannounce_check_failed();
                tracing::warn!(
                    %wallet,
                    %room,
                    reason = "gateway failure",
                    "cannot tell whether the wallet already holds its island; not re-announcing"
                );
                return false;
            }
        };
        if matches!(occupant, IslandOccupant::Unattributed)
            || matches!(&occupant, IslandOccupant::Session(owner) if owner == session)
        {
            crate::gatekeeper::admission::cluster::metrics::cluster_reannounce_suppressed();
            return false;
        }
        true
    }
}

fn snapshot_extends(expected: &PeerClusterSnapshot, current: &PeerClusterSnapshot) -> bool {
    crate::protocol::bus::cluster::selected_evidence(expected)
        && crate::protocol::bus::cluster::selected_evidence(current)
        && current.selected_origin == expected.selected_origin
        && current.selected_identity == expected.selected_identity
        && current.assignment_state == expected.assignment_state
        && current.wallet == expected.wallet
        && current.session == expected.session
        && current.assignment_version == expected.assignment_version
        && current.source_incarnation == expected.source_incarnation
        && current.pass >= expected.pass
        && current.cluster_id == expected.cluster_id
        && current.realm == expected.realm
}

#[cfg(test)]
mod tests;

impl ClusterSubscriber {
    async fn clean_durable(
        &self,
        store: &super::safety::SafetyStore,
        expected: &PeerClusterSnapshot,
        guard: Option<&RecoveryGuard>,
    ) -> bool {
        use super::safety::Settlement;
        for operation in &expected.pending_takeovers {
            if !self.operation_current(expected, operation).await {
                return false;
            }
            if store
                .prepare_takeover(
                    &expected.wallet,
                    &expected.source_incarnation,
                    &expected.session,
                    operation,
                )
                .await
                .is_err()
            {
                return false;
            }
            let room = island_room_name(&operation.displaced_cluster_id);
            let mut confirmed = None;
            let mut created = false;
            for attempt in 0..3 {
                if !self.operation_current(expected, operation).await {
                    return false;
                }
                // Scope starts before polling begin, including its commit acknowledgment.
                if guard.is_some_and(RecoveryGuard::cancelled) {
                    return false;
                }
                let _scope = guard.map(RecoveryGuard::enter);
                let Ok(Some(mut dispatch)) = store
                    .begin_takeover(
                        &expected.wallet,
                        &expected.source_incarnation,
                        operation,
                        expected.token_not_before.unwrap_or(0),
                    )
                    .await
                else {
                    return false;
                };
                if dispatch.state == DispatchState::Quarantine {
                    let Ok(Some(final_dispatch)) =
                        store.begin_quarantine_final(dispatch.dispatch_id).await
                    else {
                        return false;
                    };
                    dispatch = final_dispatch;
                }
                if dispatch.fresh {
                    // The shared immutable dispatch is committed before polling
                    // SFU RPC. Cancellation or unknown leaves every replica closed.
                    let outcome = self
                        .gateway
                        .remove_participant(&room, &expected.wallet, Some(dispatch.cutoff as i64))
                        .await;
                    use super::safety::{QuarantinePolicy, RemovalOutcome, classify_removal};
                    let classified = match outcome {
                        Ok(_) => RemovalOutcome::Positive,
                        Err(GatewayError::Unknown) => RemovalOutcome::Unknown,
                        Err(GatewayError::DefiniteRejection | GatewayError::RoomMissing) => {
                            RemovalOutcome::Rejected
                        }
                    };
                    let quarantine = if self.config.self_hosted_token_quarantine {
                        QuarantinePolicy::Enabled {
                            applies: operation.displaced_cluster_id == expected.cluster_id,
                        }
                    } else {
                        QuarantinePolicy::Disabled
                    };
                    let Some(settlement) = classify_removal(
                        classified,
                        dispatch.phase,
                        quarantine,
                        self.admission_now_ms(),
                        dispatch.cutoff,
                    ) else {
                        return false;
                    };
                    if !matches!(
                        store.settle(dispatch.dispatch_id, settlement).await,
                        Ok(true)
                    ) {
                        return false;
                    }
                    drop(_scope);
                    if guard.is_some_and(RecoveryGuard::cancelled) {
                        return false;
                    }
                    match settlement {
                        Settlement::Confirmed => dispatch.state = DispatchState::Confirmed,
                        Settlement::Quarantine(_) => return false,
                        Settlement::Rejected => {
                            if matches!(outcome, Err(GatewayError::RoomMissing)) && !created {
                                created = true;
                                if self.gateway.ensure_recovery_room(&room).await.is_err() {
                                    return false;
                                }
                            }
                            if attempt < 2 && self.config.takeover_retry_delay_ms > 0 {
                                tokio::time::sleep(Duration::from_millis(
                                    self.config.takeover_retry_delay_ms,
                                ))
                                .await;
                            }
                            continue;
                        }
                        Settlement::SettledLate => {
                            if matches!(outcome, Err(GatewayError::RoomMissing)) && !created {
                                created = true;
                                if self.gateway.ensure_recovery_room(&room).await.is_err() {
                                    return false;
                                }
                            }
                            continue;
                        }
                    }
                }
                if dispatch.state != DispatchState::Confirmed
                    || guard.is_some_and(RecoveryGuard::cancelled)
                {
                    return false;
                }
                confirmed = Some(dispatch);
                break;
            }
            let Some(dispatch) = confirmed else {
                return false;
            };
            if !self.operation_current(expected, operation).await {
                return false;
            }
            let completion = crate::protocol::bus::cluster::Completion {
                wallet: expected.wallet.clone(),
                completion: crate::protocol::pulse::decentraland::pulse::TakeoverCompleted {
                    takeover_id: operation.takeover_id.clone(),
                    revoke_before: dispatch.cutoff,
                },
            };
            let _ = self
                .bus
                .publish(&completion.subject(), completion.completion.encode_to_vec())
                .await;
            let Some(current) = self.same_authority(expected).await else {
                return false;
            };
            if current.pending_takeovers.contains(operation) {
                return false;
            }
            if store
                .acknowledge(
                    &current.wallet,
                    &current.source_incarnation,
                    &current.pending_takeovers,
                )
                .await
                .is_err()
            {
                return false;
            }
        }
        true
    }
}
impl ClusterSubscriber {
    pub(crate) async fn replay_due(self: &Arc<Self>) {
        let Some(store) = &self.safety else {
            return;
        };
        match store.due_takeovers().await {
            Ok(due) => {
                for (wallet, session) in due {
                    self.announce_connect(&wallet, &session);
                }
            }
            Err(_) => tracing::warn!(
                "island durable recovery scheduling unavailable; admission remains closed"
            ),
        }
    }
}
