use crate::archipelago::feed::FeedCache;
use crate::archipelago::registry::PeerLink;
use crate::gatekeeper::admission::cluster::fence::TokenFence;
use axum::extract::ws::Message;
use futures::{Sink, SinkExt};
use std::time::Duration;

const WRITE_DEADLINE: Duration = Duration::from_secs(5);

/// A failed/cancelled write may already have emitted a frame prefix. The caller
/// must retire the transport, never retry the frame or reuse that sink.
async fn write<S: Sink<Message> + Unpin>(
    socket: &mut S,
    message: Message,
    owner: Option<&PeerLink>,
    feed: &FeedCache,
) -> bool {
    let closed = async {
        match owner {
            Some(owner) => owner.cancelled().await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        biased;
        _ = closed => false,
        _ = tokio::time::sleep(WRITE_DEADLINE) => {
            feed.on_socket_write_timeout();
            false
        }
        result = socket.send(message) => result.is_ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_heartbeat_encodings_receive_identical_rate_accounting() {
        // All three packets carry x=1. The last two terminate after that heartbeat.
        for bytes in [
            vec![26, 7, 10, 5, 13, 0, 0, 128, 63],
            vec![26, 7, 10, 5, 13, 0, 0, 128, 63, 0],
            vec![26, 7, 10, 5, 13, 0, 0, 128, 63, 36],
        ] {
            let epoch = Instant::now();
            let mut rate = HeartbeatRate::default();
            let limited = (0..=1100).any(|tick| {
                is_heartbeat(&bytes) && rate.sample(epoch + Duration::from_millis(tick * 10))
            });
            assert!(
                limited,
                "accepted heartbeat must enter sustained-rate accounting"
            );
        }
    }
    #[test]
    fn idle_probe_grace_and_activity_follow_transport_sweep_boundaries() {
        let epoch = Instant::now();
        let mut idle = Idle::new(epoch, epoch + Duration::from_secs(3));
        assert_eq!(idle.deadline, epoch + Duration::from_secs(76));
        assert!(!idle.expired(idle.deadline));
        assert_eq!(idle.deadline, epoch + Duration::from_secs(92));
        assert!(idle.expired(idle.deadline));
        for reset in [70, 75, 91] {
            idle.activity(epoch + Duration::from_secs(reset));
            assert!(!idle.probing);
            let ping = epoch + Duration::from_secs((reset / 4 + 19) * 4);
            assert_eq!(idle.deadline, ping);
            assert!(!idle.expired(ping));
            assert!(!idle.probing || idle.deadline == ping + Duration::from_secs(16));
        }
    }
    use crate::archipelago::registry::{PeersRegistry, SocketEvent};
    use std::pin::Pin;
    use std::task::{Context, Poll};

    #[derive(Default)]
    struct HeldSink {
        ready: bool,
        flush: bool,
        started: usize,
    }

    impl Sink<Message> for HeldSink {
        type Error = ();
        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), ()>> {
            if self.ready {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }
        fn start_send(mut self: Pin<&mut Self>, _: Message) -> Result<(), ()> {
            self.started += 1;
            Ok(())
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), ()>> {
            if self.flush {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }
        fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), ()>> {
            self.poll_flush(cx)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn writes_are_bounded_before_readiness_and_during_flush() {
        for ready in [false, true] {
            let feed = FeedCache::default();
            let mut sink = HeldSink {
                ready,
                ..Default::default()
            };
            let started = tokio::time::Instant::now();
            assert!(!write(&mut sink, Message::Binary(vec![1].into()), None, &feed).await);
            assert_eq!(tokio::time::Instant::now() - started, WRITE_DEADLINE);
            assert_eq!(sink.started, usize::from(ready));
            assert_eq!(feed.socket_write_timeout_count(), 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn closed_owners_never_begin_a_new_frame_even_on_a_ready_sink() {
        let registry = PeersRegistry::new();
        let (owner, _rx, _) = registry.on_peer_connected("wallet", "session");
        owner.close();
        let feed = FeedCache::default();
        let mut sink = HeldSink {
            ready: true,
            flush: true,
            started: 0,
        };
        assert!(
            !write(
                &mut sink,
                Message::Binary(vec![1].into()),
                Some(&owner),
                &feed
            )
            .await
        );
        assert_eq!(sink.started, 0);
        assert_eq!(feed.socket_write_timeout_count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_kick_or_close_interrupts_a_pending_frame_without_waiting_for_the_deadline() {
        for kick in [false, true] {
            let registry = PeersRegistry::new();
            let (owner, _rx, _) = registry.on_peer_connected("wallet", "session");
            let feed = FeedCache::default();
            let mut sink = HeldSink {
                ready: true,
                ..Default::default()
            };
            let started = tokio::time::Instant::now();
            let mut write = Box::pin(write(
                &mut sink,
                Message::Binary(vec![1].into()),
                Some(&owner),
                &feed,
            ));
            assert!(futures::poll!(&mut write).is_pending());
            if kick {
                owner.send(SocketEvent::Kicked);
            } else {
                owner.close();
            }
            assert!(!write.await);
            assert_eq!(tokio::time::Instant::now(), started);
            assert_eq!(feed.socket_write_timeout_count(), 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_current_owner_can_finish_the_write() {
        let registry = PeersRegistry::new();
        let (owner, _rx, _) = registry.on_peer_connected("wallet", "session");
        let feed = FeedCache::default();
        let mut sink = HeldSink {
            ready: true,
            flush: true,
            started: 0,
        };
        assert!(
            write(
                &mut sink,
                Message::Binary(vec![1].into()),
                Some(&owner),
                &feed
            )
            .await
        );
        assert_eq!(sink.started, 1);
        assert_eq!(feed.socket_write_timeout_count(), 0);
    }
}

use crate::archipelago::control_v4::V4Owner;
use crate::archipelago::state::AppState;
use axum::extract::ws::{CloseFrame, WebSocket as Transport};
use futures::StreamExt;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{Notify, OwnedSemaphorePermit};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const QUEUE_CAPACITY: usize = 64;
const FULL_DEADLINE: Duration = Duration::from_secs(10);
const POSITION_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Default)]
struct Pending {
    frames: VecDeque<Frame>,
    terminal: Option<Terminal>,
    full_since: Option<Instant>,
    owner: Option<PeerLink>,
    done: bool,
    write_phase: WritePhase,
}
#[derive(Default, PartialEq, Eq)]
enum WritePhase {
    #[default]
    Clean,
    Active,
    Retired,
}
impl Pending {
    fn pop(&mut self) -> Option<Frame> {
        let frame = self.frames.pop_front();
        if self.frames.len() < QUEUE_CAPACITY {
            self.full_since = None;
        }
        frame
    }
    fn full_expired(&self, now: Instant) -> bool {
        self.frames.len() == QUEUE_CAPACITY
            && self.full_since.is_some_and(|at| now >= at + FULL_DEADLINE)
    }
    // A full queue hands the frame back; boxing it would allocate on the send path.
    #[allow(clippy::result_large_err)]
    fn push(&mut self, frame: Frame) -> Result<(), Frame> {
        if frame.assignment
            && let Some(index) = self.frames.iter().position(|held| held.assignment)
        {
            self.frames[index] = frame;
            return Ok(());
        }
        if self.frames.len() == QUEUE_CAPACITY {
            return Err(frame);
        }
        self.frames.push_back(frame);
        if self.frames.len() == QUEUE_CAPACITY {
            self.full_since = Some(Instant::now());
        }
        Ok(())
    }
}
/// A packet and its admission evidence enter the queue as one value.
pub(super) enum Outgoing {
    Packet(Message),
    Assignment {
        message: Message,
        stamp: Option<crate::gatekeeper::admission::cluster::fence::PulseAdmissionStamp>,
    },
    ConfiguredAssignment {
        message: Message,
        stamp: crate::gatekeeper::admission::cluster::fence::PulseAdmissionStamp,
        token_fence: TokenFence,
        owner: V4Owner,
    },
    Kick(Message),
}

struct Frame {
    message: Message,
    owner: Option<PeerLink>,
    delivery: Option<(Option<bool>, bool)>,
    assignment: bool,
    pulse_admission: Option<crate::gatekeeper::admission::cluster::fence::PulseAdmissionStamp>,
    island_token_fence: Option<TokenFence>,
    permit: Option<crate::archipelago::registry::ControlPositionPermit>,
    fence_bound: bool,
}
enum Terminal {
    Kick(Message),
    Close(u16, String),
    Abort,
}
#[derive(Default)]
struct Queue {
    pending: Mutex<Pending>,
    changed: Notify,
    closed: CancellationToken,
    peer_close: CancellationToken,
}
impl Queue {
    fn clean_peer_close(&self) -> bool {
        self.peer_close.is_cancelled() && self.pending.lock().write_phase == WritePhase::Clean
    }
    fn peer_closed(&self) {
        let mut pending = self.pending.lock();
        pending.frames.clear();
        pending.full_since = None;
        pending.terminal = None;
        pending.done = true;
        if pending.write_phase == WritePhase::Active {
            pending.write_phase = WritePhase::Retired;
        }
        self.peer_close.cancel();
        self.changed.notify_waiters();
    }

    fn terminal(&self, reason: Terminal) {
        let mut pending = self.pending.lock();
        if self.peer_close.is_cancelled() {
            return;
        }
        if !matches!(pending.terminal, Some(Terminal::Kick(_)))
            && (!matches!(pending.terminal, Some(Terminal::Close(1012, _)))
                || matches!(reason, Terminal::Kick(_)))
        {
            pending.terminal = Some(reason);
        }
        self.changed.notify_waiters();
    }
}

/// One bounded transport adapter. Writer owns the permit until actual termination.
pub(super) struct WebSocket {
    incoming: futures::stream::SplitStream<Transport>,
    queue: Arc<Queue>,
    state: AppState,
    rate: HeartbeatRate,
    sample: Option<PositionSample>,
    sample_generation: u64,
    next_position: Instant,
    idle: Idle,
}

/// Four-second transport sweeps: probe after 19 ticks, retire after four more.
/// Activity here means complete messages, including Ping/Pong, exposed by Axum.
struct Idle {
    epoch: Instant,
    deadline: Instant,
    probing: bool,
}
impl Idle {
    fn new(epoch: Instant, now: Instant) -> Self {
        let mut idle = Self {
            epoch,
            deadline: now,
            probing: false,
        };
        idle.activity(now);
        idle
    }
    fn activity(&mut self, now: Instant) {
        self.probing = false;
        self.deadline = self.epoch
            + Duration::from_secs(
                (now.saturating_duration_since(self.epoch).as_secs() / 4 + 19) * 4,
            );
    }
    fn expired(&mut self, now: Instant) -> bool {
        if self.probing {
            return true;
        }
        self.probing = true;
        self.deadline = now + Duration::from_secs(16);
        false
    }
}
#[derive(Clone)]
struct PositionSample {
    generation: u64,
    permit: Option<crate::archipelago::registry::ControlPositionPermit>,
    owner: Option<V4Owner>,
    link: PeerLink,
    realm: String,
    position: [f32; 3],
}

impl WebSocket {
    pub(super) fn new(socket: Transport, state: AppState, permit: OwnedSemaphorePermit) -> Self {
        let (sink, incoming) = socket.split();
        let queue = Arc::new(Queue::default());
        tokio::spawn(writer(sink, queue.clone(), state.clone(), permit));
        Self {
            incoming,
            queue,
            idle: Idle::new(Instant::from_std(state.started_at), Instant::now()),
            state,
            rate: HeartbeatRate::default(),
            sample: None,
            sample_generation: 0,
            next_position: Instant::now(),
        }
    }
    pub(super) fn bind_owner(&mut self, owner: &PeerLink) {
        self.queue.pending.lock().owner = Some(owner.clone());
        self.queue.changed.notify_waiters();
    }
    pub(super) async fn position(
        &mut self,
        owner: Option<V4Owner>,
        link: PeerLink,
        realm: String,
        position: [f32; 3],
    ) {
        self.bind_owner(&link);
        self.sample_generation = self.sample_generation.wrapping_add(1);
        let permit = owner.as_ref().and_then(|owner| {
            self.state.registry.control_position_permit(
                &owner.address,
                &owner.session,
                owner.epoch,
                false,
            )
        });
        self.sample = Some(PositionSample {
            generation: self.sample_generation,
            permit,
            owner,
            link,
            realm,
            position,
        });
        if Instant::now() >= self.next_position {
            self.apply_position().await;
        }
    }
    async fn apply_position(&mut self) {
        let checking = self.state.clone();
        let applying = self.state.clone();
        commit_position(
            &mut self.sample,
            &mut self.next_position,
            move |sample| async move {
                match &sample.owner {
                    Some(owner) => matches!(
                        tokio::time::timeout(
                            super::AUTHORITY_OPERATION_TIMEOUT,
                            super::legacy_fence::owns_realm(&checking, owner)
                        )
                        .await,
                        Ok(Ok(true))
                    ),
                    None => true,
                }
            },
            move |sample| {
                if let Some(owner) = &sample.owner {
                    if !sample.permit.as_ref().is_some_and(|permit| {
                        applying.registry.control_position_permit_is_current(permit)
                    }) {
                        return false;
                    }
                    super::control_position::publish(
                        &applying,
                        owner,
                        &sample.realm,
                        Some(sample.position),
                    );
                }
                true
            },
        )
        .await;
    }
    pub(super) async fn recv(&mut self) -> Option<Result<Message, axum::Error>> {
        loop {
            if self.queue.peer_close.is_cancelled() {
                return None;
            }
            tokio::select! {
                biased;
                _ = self.state.restart.cancelled() => return None,
                _ = self.queue.closed.cancelled() => return None,
                _ = tokio::time::sleep_until(self.idle.deadline) => {
                    if self.idle.expired(Instant::now()) {
                        self.queue.terminal(Terminal::Abort);
                        return None;
                    }
                    if !send(self, Message::Ping(bytes::Bytes::new()), None).await {
                        return None;
                    }
                }
                _ = tokio::time::sleep_until(self.next_position), if self.sample.is_some() => self.apply_position().await,
                message = self.incoming.next() => {
                    if matches!(message, Some(Ok(Message::Close(_)))) {
                        self.sample = None;
                        self.queue.peer_closed();
                        return message;
                    }
                    if matches!(message, Some(Ok(_))) { self.idle.activity(Instant::now()); }
                    // Application data uses the same decoder and rate accounting for both opcodes.
                    let message = message.map(|result| result.map(|message| match message {
                        Message::Text(text) => Message::Binary(text.as_bytes().to_vec().into()),
                        other => other,
                    }));
                    let Some(Ok(Message::Binary(bytes))) = message.as_ref() else { return message; };
                    if is_heartbeat(bytes) && self.rate.sample(Instant::now()) {
                        self.queue.terminal(Terminal::Close(1008, String::new())); return None;
                    }
                    return message;
                }
            }
        }
    }
}
/// The pending value stays in socket-owned storage across the cancellable authority wait.
/// Only a completed check may discard it; only a committed application advances cadence.
async fn commit_position<F, C, A>(
    pending: &mut Option<PositionSample>,
    next: &mut Instant,
    check: C,
    apply: A,
) where
    F: std::future::Future<Output = bool>,
    C: FnOnce(PositionSample) -> F,
    A: FnOnce(PositionSample) -> bool,
{
    if Instant::now() < *next {
        return;
    }
    let Some(sample) = pending.as_ref().cloned() else {
        return;
    };
    let valid = !sample.link.is_closed() && check(sample.clone()).await;
    if !pending
        .as_ref()
        .is_some_and(|latest| latest.generation == sample.generation)
    {
        return;
    }
    // Cancellation before this point leaves both the retained sample and deadline untouched.
    pending.take();
    if valid && !sample.link.is_closed() && apply(sample) {
        *next = Instant::now() + POSITION_INTERVAL;
    }
}

impl Drop for WebSocket {
    fn drop(&mut self) {
        self.queue.pending.lock().done = true;
        self.queue.changed.notify_waiters();
    }
}

pub(super) async fn send(
    socket: &mut WebSocket,
    message: Message,
    owner: Option<&PeerLink>,
) -> bool {
    send_outgoing(socket, Outgoing::Packet(message), owner).await
}

pub(super) async fn send_outgoing(
    socket: &mut WebSocket,
    outgoing: Outgoing,
    owner: Option<&PeerLink>,
) -> bool {
    if socket.queue.peer_close.is_cancelled() {
        return false;
    }
    let (message, assignment, pulse_admission, island_token_fence, fence) = match outgoing {
        Outgoing::Kick(message) => {
            socket.queue.terminal(Terminal::Kick(message));
            return !socket.queue.peer_close.is_cancelled();
        }
        Outgoing::Packet(message) => (message, false, None, None, None),
        Outgoing::Assignment { message, stamp } => (message, true, stamp, None, None),
        Outgoing::ConfiguredAssignment {
            message,
            stamp,
            token_fence,
            owner,
        } => (message, true, Some(stamp), Some(token_fence), Some(owner)),
    };
    if let Message::Close(frame) = &message {
        socket.queue.terminal(Terminal::Close(
            frame.as_ref().map_or(1000, |frame| frame.code),
            frame
                .as_ref()
                .map_or_else(String::new, |frame| frame.reason.to_string()),
        ));
        return !socket.queue.peer_close.is_cancelled();
    }
    let fence_bound = fence.is_some();
    let permit = fence.as_ref().and_then(|owner| {
        socket.state.registry.control_position_permit(
            &owner.address,
            &owner.session,
            owner.epoch,
            false,
        )
    });
    let mut frame = Some(Frame {
        message,
        owner: owner.cloned(),
        delivery: assignment.then_some((Some(true), true)),
        assignment,
        pulse_admission,
        island_token_fence,
        permit,
        fence_bound,
    });
    loop {
        let changed = socket.queue.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let deadline = {
            let mut pending = socket.queue.pending.lock();
            if socket.state.restart.is_cancelled()
                || pending.terminal.is_some()
                || pending.done
                || socket.queue.closed.is_cancelled()
            {
                return false;
            }
            if owner.is_some_and(PeerLink::is_closed) {
                if owner.is_some_and(PeerLink::was_kicked) {
                    drop(pending);
                    socket.queue.terminal(Terminal::Kick(kick_message()));
                }
                return false;
            }
            if let Some(owner) = owner {
                pending.owner = Some(owner.clone());
            }
            match pending.push(frame.take().unwrap()) {
                Ok(()) => {
                    socket.queue.changed.notify_waiters();
                    return true;
                }
                Err(held) => frame = Some(held),
            }
            *pending.full_since.get_or_insert_with(Instant::now) + FULL_DEADLINE
        };
        tokio::select! {
            _ = socket.state.restart.cancelled() => return false,
            _ = socket.queue.closed.cancelled() => return false,
            _ = async { if let Some(owner) = owner { owner.cancelled().await } else { std::future::pending().await } } => {
                if owner.is_some_and(PeerLink::was_kicked) { socket.queue.terminal(Terminal::Kick(kick_message())); }
                return false;
            },
            _ = changed => {},
            _ = tokio::time::sleep_until(deadline) => {
                let full = { let pending = socket.queue.pending.lock(); pending.full_expired(Instant::now()) };
                if full { socket.queue.terminal(Terminal::Close(1013, "Island assignment dropped; reconnect".into())); return false; }
            }
        }
    }
}

async fn writer<S: Sink<Message> + Unpin>(
    mut sink: S,
    queue: Arc<Queue>,
    state: AppState,
    _permit: OwnedSemaphorePermit,
) {
    loop {
        let changed = queue.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let peer_close = {
            let mut pending = queue.pending.lock();
            if queue.peer_close.is_cancelled() {
                let clean = pending.write_phase == WritePhase::Clean;
                pending.write_phase = WritePhase::Retired;
                Some(clean)
            } else {
                None
            }
        };
        if let Some(clean) = peer_close {
            // Every completed send has flushed both SplitSink's slot and the
            // transport buffers. Only the parser's automatic reply remains.
            if clean {
                if tokio::time::timeout(WRITE_DEADLINE, sink.flush())
                    .await
                    .is_err()
                {
                    state.feed.on_socket_write_timeout();
                }
            }
            break;
        }
        let (frame, terminal, done) = {
            let mut pending = queue.pending.lock();
            if queue.peer_close.is_cancelled() {
                continue;
            }
            if pending.owner.as_ref().is_some_and(PeerLink::was_kicked)
                && !matches!(pending.terminal, Some(Terminal::Kick(_)))
            {
                pending.terminal = Some(Terminal::Kick(kick_message()));
            }
            if state.restart.is_cancelled() && !matches!(pending.terminal, Some(Terminal::Kick(_)))
            {
                pending.terminal = Some(Terminal::Close(1012, String::new()));
            }
            if pending.full_expired(Instant::now()) && pending.terminal.is_none() {
                pending.terminal = Some(Terminal::Close(
                    1013,
                    "Island assignment dropped; reconnect".into(),
                ));
            }
            let terminal = pending.terminal.take();
            let frame = if terminal.is_none() {
                pending.pop()
            } else {
                pending.frames.clear();
                None
            };
            if pending.frames.len() < QUEUE_CAPACITY {
                pending.full_since = None;
            }
            (frame, terminal, pending.done)
        };
        if frame.is_some() || terminal.is_some() {
            queue.changed.notify_waiters();
        }
        if let Some(terminal) = terminal {
            if matches!(terminal, Terminal::Abort) {
                break;
            }
            if let Terminal::Kick(message) = terminal {
                if !coordinated_write(&mut sink, message, &queue, &state.feed).await {
                    if queue.clean_peer_close() {
                        continue;
                    }
                    break;
                }
                let _ =
                    coordinated_write(&mut sink, Message::Close(None), &queue, &state.feed).await;
            } else if let Terminal::Close(code, reason) = terminal {
                if code == 1013 {
                    state.feed.on_dropped_close();
                }
                let _ = coordinated_write(
                    &mut sink,
                    Message::Close(Some(CloseFrame {
                        code,
                        reason: reason.into(),
                    })),
                    &queue,
                    &state.feed,
                )
                .await;
            }
            if queue.clean_peer_close() {
                continue;
            }
            break;
        }
        if let Some(frame) = frame {
            if let Some(stamp) = frame.pulse_admission.as_ref() {
                let validation = async {
                    matches!(
                        tokio::time::timeout(super::AUTHORITY_OPERATION_TIMEOUT, async {
                            match (frame.island_token_fence.as_ref(), frame.fence_bound) {
                                (Some(fence), true) => {
                                    state.gatekeeper.verify_island_stamp(stamp).await
                                        && state
                                            .gatekeeper
                                            .island_fenced_stamp_ready_sql(stamp, fence, None)
                                            .await
                                }
                                (None, false) => state.gatekeeper.verify_island_stamp(stamp).await,
                                // A configured credential retains its exact committed
                                // fence through the queue, alongside the original stamp.
                                _ => false,
                            }
                        })
                        .await,
                        Ok(true)
                    )
                };
                let valid = tokio::select! {
                    biased;
                    _ = queue.peer_close.cancelled() => { continue; },
                    valid = validation => valid,
                };
                if !valid
                    || !frame.assignment
                    || if frame.fence_bound {
                        !frame.permit.as_ref().is_some_and(|permit| {
                            permit.matches_identity(&stamp.wallet, &stamp.session)
                        })
                    } else {
                        frame
                            .owner
                            .as_ref()
                            .is_none_or(|owner| owner.session != stamp.session)
                    }
                {
                    if let Some(owner) = &frame.owner {
                        owner.assignment_cleared();
                    }
                    state
                        .gatekeeper
                        .announce_session(&stamp.wallet, &stamp.session);
                    continue;
                }
            }
            // The authority wait cannot substitute for the exact local owner/fence check.
            if frame.owner.as_ref().is_some_and(PeerLink::is_closed)
                || (frame.fence_bound
                    && !frame.permit.as_ref().is_some_and(|permit| {
                        state.registry.control_position_permit_is_current(permit)
                    }))
            {
                continue;
            }
            if state.restart.is_cancelled() {
                continue;
            }
            if frame.fence_bound {
                let Some(stamp) = frame.pulse_admission.as_ref() else {
                    continue;
                };
                let Some(permit) = frame.permit.as_ref() else {
                    continue;
                };
                if !state.registry.with_control_position_permit(permit, || {
                    state.enforcement.set_island(
                        &stamp.wallet,
                        &crate::rooms::island_room_name(&stamp.cluster_id),
                    );
                }) {
                    continue;
                }
            }
            // Never cancel a partial frame to send another frame. Physical failure retires the sink.
            if !coordinated_write(&mut sink, frame.message, &queue, &state.feed).await {
                if queue.clean_peer_close() {
                    continue;
                }
                break;
            }
            if let Some((assigned, metric)) = frame.delivery
                && let Some(owner) = frame.owner.as_ref().filter(|owner| {
                    !owner.is_closed()
                        && (!frame.fence_bound
                            || frame.permit.as_ref().is_some_and(|permit| {
                                state.registry.control_position_permit_is_current(permit)
                            }))
                })
            {
                match assigned {
                    Some(true) => owner.assignment_sent(),
                    Some(false) => owner.assignment_cleared(),
                    None => {}
                }
                if metric {
                    state.feed.on_socket_assignment_written();
                }
            }
        } else if done {
            let _ = coordinated_write(&mut sink, Message::Close(None), &queue, &state.feed).await;
            if queue.clean_peer_close() {
                continue;
            }
            break;
        } else {
            tokio::select! { _ = changed => {}, _ = state.restart.cancelled() => {} }
        }
    }
    queue.closed.cancel();
}

async fn coordinated_write<S: Sink<Message> + Unpin>(
    sink: &mut S,
    message: Message,
    queue: &Queue,
    feed: &FeedCache,
) -> bool {
    {
        let mut pending = queue.pending.lock();
        // Publication and admission to the physical send share this lock. No
        // mutex is held while polling the sink or waiting for cancellation.
        if queue.peer_close.is_cancelled() || pending.write_phase != WritePhase::Clean {
            return false;
        }
        pending.write_phase = WritePhase::Active;
    }
    let success = tokio::select! {
        biased;
        _ = queue.peer_close.cancelled() => false,
        success = write(sink, message, None, feed) => success,
    };
    let mut pending = queue.pending.lock();
    if success && pending.write_phase == WritePhase::Active {
        pending.write_phase = WritePhase::Clean;
        true
    } else {
        pending.write_phase = WritePhase::Retired;
        false
    }
}

#[derive(Default)]
struct HeartbeatRate {
    recent: VecDeque<Instant>,
    excess_since: Option<Instant>,
}
impl HeartbeatRate {
    /// Rolling one-second window. Any sample with at most20 in-window resets excess continuity.
    fn sample(&mut self, now: Instant) -> bool {
        while self
            .recent
            .front()
            .is_some_and(|at| now.duration_since(*at) >= Duration::from_secs(1))
        {
            self.recent.pop_front();
        }
        if self.recent.len() == 21 {
            self.recent.pop_front();
        }
        self.recent.push_back(now);
        if self.recent.len() > 20 {
            now.duration_since(*self.excess_since.get_or_insert(now)) >= Duration::from_secs(10)
        } else {
            self.excess_since = None;
            false
        }
    }
}
fn is_heartbeat(bytes: &[u8]) -> bool {
    use crate::protocol::archipelago::archipelago::client_packet;
    super::legacy_decode::decode(bytes)
        .is_ok_and(|packet| matches!(packet.message, Some(client_packet::Message::Heartbeat(_))))
}

fn kick_message() -> Message {
    Message::Binary(super::craft(super::kicked_packet()).into())
}

#[cfg(test)]
mod bounds_tests {
    use super::*;
    use crate::archipelago::registry::{PeersRegistry, SocketEvent};
    use std::pin::Pin;
    use std::task::{Context, Poll};
    struct Recorder {
        frames: Arc<Mutex<Vec<Message>>>,
        flush: bool,
    }
    impl Sink<Message> for Recorder {
        type Error = ();
        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), ()>> {
            Poll::Ready(Ok(()))
        }
        fn start_send(self: Pin<&mut Self>, frame: Message) -> Result<(), ()> {
            self.frames.lock().push(frame);
            Ok(())
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), ()>> {
            if self.flush {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }
        fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), ()>> {
            self.poll_flush(cx)
        }
    }
    async fn state() -> AppState {
        crate::archipelago::test_support::state_over(
            crate::archipelago::Settings::for_tests(),
            Arc::new(crate::gatekeeper::bus::DisabledBus),
        )
        .await
    }
    #[derive(Default)]
    struct WireProbe {
        ready: bool,
        flushed: bool,
        fail: bool,
        bytes: Vec<u8>,
        flushes: usize,
        closes: usize,
    }
    struct Probe(Arc<Mutex<WireProbe>>);
    impl futures::Stream for Probe {
        type Item = Result<Message, ()>;
        fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }
    impl Sink<Message> for Probe {
        type Error = ();
        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), ()>> {
            if self.0.lock().ready {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }
        fn start_send(self: Pin<&mut Self>, _: Message) -> Result<(), ()> {
            // Expose a physical frame prefix. A later flush would drain the secret.
            self.0.lock().bytes.extend_from_slice(&[0x82, 6]);
            Ok(())
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), ()>> {
            let mut wire = self.0.lock();
            wire.flushes += 1;
            if wire.fail {
                return Poll::Ready(Err(()));
            }
            if !wire.flushed {
                return Poll::Pending;
            }
            if wire.bytes.last() == Some(&6) {
                wire.bytes.extend_from_slice(b"secret");
            }
            Poll::Ready(Ok(()))
        }
        fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), ()>> {
            self.0.lock().closes += 1;
            self.poll_flush(cx)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn peer_close_cancels_production_readiness_slot_and_partial_frame_without_drain() {
        for (split, ready) in [(false, false), (true, false), (false, true)] {
            let state = state().await;
            let queue = Arc::new(Queue::default());
            queue
                .pending
                .lock()
                .push(frame(None, false))
                .unwrap_or_else(|_| panic!("space"));
            let wire = Arc::new(Mutex::new(WireProbe {
                ready,
                ..Default::default()
            }));
            let permits = Arc::new(tokio::sync::Semaphore::new(1));
            let permit = permits.clone().acquire_owned().await.unwrap();
            let sink = Probe(wire.clone());
            let mut writing: Pin<Box<dyn std::future::Future<Output = ()>>> = if split {
                let (sink, _read) = sink.split();
                Box::pin(writer(sink, queue.clone(), state.clone(), permit))
            } else {
                Box::pin(writer(sink, queue.clone(), state.clone(), permit))
            };
            let at = Instant::now();
            assert!(futures::poll!(&mut writing).is_pending());
            assert_eq!(permits.available_permits(), 0);
            let flushes = wire.lock().flushes;
            queue.peer_closed();
            // Even a now-writable transport must not drain a retained credential.
            {
                let mut wire = wire.lock();
                wire.ready = true;
                wire.flushed = true;
            }
            writing.await;
            assert_eq!(
                Instant::now(),
                at,
                "peer cancellation must beat the write deadline"
            );
            let wire = wire.lock();
            assert_eq!(wire.bytes, if ready { vec![0x82, 6] } else { vec![] });
            assert_eq!(wire.flushes, flushes);
            assert_eq!(wire.closes, 0);
            assert_eq!(state.feed.socket_write_timeout_count(), 0);
            assert_eq!(permits.available_permits(), 1);
            assert!(queue.closed.is_cancelled());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn clean_peer_close_discards_queue_and_preempts_kick_restart_without_start_send() {
        let state = state().await;
        let queue = Arc::new(Queue::default());
        let (owner, _events, _) = state.registry.on_peer_connected("wallet", "session");
        {
            let mut pending = queue.pending.lock();
            pending.owner = Some(owner.clone());
            pending
                .push(frame(Some(owner.clone()), true))
                .unwrap_or_else(|_| panic!("space"));
        }
        queue.terminal(Terminal::Kick(kick_message()));
        queue.peer_closed();
        owner.send(SocketEvent::Kicked);
        state.restart.cancel();
        queue.terminal(Terminal::Close(1012, String::new()));
        let wire = Arc::new(Mutex::new(WireProbe {
            ready: true,
            flushed: true,
            ..Default::default()
        }));
        let permits = Arc::new(tokio::sync::Semaphore::new(1));
        writer(
            Probe(wire.clone()),
            queue.clone(),
            state,
            permits.clone().acquire_owned().await.unwrap(),
        )
        .await;
        let wire = wire.lock();
        assert!(wire.bytes.is_empty());
        assert_eq!(wire.flushes, 1);
        assert_eq!(wire.closes, 0);
        assert!(queue.pending.lock().frames.is_empty());
        assert_eq!(permits.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_clean_control_flush_releases_permit_once_at_existing_deadline() {
        let state = state().await;
        let queue = Arc::new(Queue::default());
        queue.peer_closed();
        let wire = Arc::new(Mutex::new(WireProbe::default()));
        let permits = Arc::new(tokio::sync::Semaphore::new(1));
        let at = Instant::now();
        let mut job = Box::pin(writer(
            Probe(wire.clone()),
            queue.clone(),
            state.clone(),
            permits.clone().acquire_owned().await.unwrap(),
        ));
        assert!(futures::poll!(&mut job).is_pending());
        assert_eq!(permits.available_permits(), 0);
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(futures::poll!(&mut job).is_pending());
        assert_eq!(permits.available_permits(), 0);
        tokio::time::advance(Duration::from_secs(1)).await;
        job.await;
        assert_eq!(Instant::now() - at, WRITE_DEADLINE);
        assert!(wire.lock().bytes.is_empty());
        assert_eq!(wire.lock().closes, 0);
        assert_eq!(state.feed.socket_write_timeout_count(), 1);
        assert_eq!(permits.available_permits(), 1);
        assert!(queue.closed.is_cancelled());
    }

    #[tokio::test]
    async fn peer_close_wakes_idle_production_writer_without_a_lost_notification() {
        let state = state().await;
        let queue = Arc::new(Queue::default());
        let wire = Arc::new(Mutex::new(WireProbe {
            flushed: true,
            ..Default::default()
        }));
        let permit = Arc::new(tokio::sync::Semaphore::new(1))
            .acquire_owned()
            .await
            .unwrap();
        let job = tokio::spawn(writer(Probe(wire.clone()), queue.clone(), state, permit));
        tokio::task::yield_now().await;
        queue.peer_closed();
        tokio::time::timeout(Duration::from_secs(1), job)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(wire.lock().flushes, 1);
        assert!(wire.lock().bytes.is_empty());
    }

    struct HeldAuthority {
        bus: crate::gatekeeper::bus::InProcessBus,
        entered: tokio::sync::Semaphore,
        release: tokio::sync::Semaphore,
        reply: Vec<u8>,
    }
    #[async_trait::async_trait]
    impl crate::gatekeeper::bus::NatsBus for HeldAuthority {
        async fn statistics_control(
            &self,
            payload: Vec<u8>,
        ) -> Result<Vec<u8>, crate::gatekeeper::bus::RequestError> {
            self.bus.statistics_control(payload).await
        }
        fn is_enabled(&self) -> bool {
            true
        }
        fn is_connected(&self) -> bool {
            true
        }
        fn is_ready(&self) -> bool {
            true
        }
        fn connect(&self) {}
        fn subscribe(
            &self,
            subject: &str,
            queue: Option<&str>,
            handler: crate::gatekeeper::bus::NatsHandler,
        ) -> crate::gatekeeper::bus::NatsSubscription {
            self.bus.subscribe(subject, queue, handler)
        }
        async fn publish(
            &self,
            subject: &str,
            payload: Vec<u8>,
        ) -> crate::gatekeeper::bus::PublishOutcome {
            self.bus.publish(subject, payload).await
        }
        async fn request(
            &self,
            _: &str,
            _: Vec<u8>,
        ) -> Result<Vec<u8>, crate::gatekeeper::bus::RequestError> {
            self.entered.add_permits(1);
            self.release.acquire().await.unwrap().forget();
            Ok(self.reply.clone())
        }
    }

    #[tokio::test]
    async fn peer_close_during_production_authority_wait_never_begins_the_held_credential() {
        use crate::gatekeeper::admission::cluster::{
            fence::PulseAdmissionStamp, peer_state::ClusterPeerState, subscriber::ClusterSubscriber,
        };
        use prost::Message as _;
        let snapshot = crate::protocol::pulse::decentraland::pulse::PeerClusterSnapshot {
            selected_origin: 1,
            selected_identity: Some(1),
            assignment_state: 1,
            wallet: "0x1111111111111111111111111111111111111111".into(),
            session: "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            cluster_id: "island".into(),
            realm: "realm".into(),
            source_incarnation: "source".into(),
            assignment_version: 1,
            token_not_before: Some(0),
            pass: 1,
            pending_takeovers: vec![],
        };
        let bus = Arc::new(HeldAuthority {
            bus: Default::default(),
            entered: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
            reply: snapshot.encode_to_vec(),
        });
        let state = crate::archipelago::test_support::state_over(
            crate::archipelago::Settings::for_tests(),
            bus.clone(),
        )
        .await;
        let subscriber = ClusterSubscriber::new(
            bus.clone(),
            Arc::new(crate::archipelago::test_support::OpenGateway),
            Arc::new(ClusterPeerState::default()),
            Default::default(),
        );
        state
            .gatekeeper
            .attach_island_subscriber_for_tests(&subscriber);
        let (owner, _events, _) = state
            .registry
            .on_peer_connected(&snapshot.wallet, &snapshot.session);
        let queue = Arc::new(Queue::default());
        let mut credential = frame(Some(owner.clone()), true);
        credential.pulse_admission = PulseAdmissionStamp::from_snapshot(&snapshot, 0);
        queue
            .pending
            .lock()
            .push(credential)
            .unwrap_or_else(|_| panic!("space"));
        let wire = Arc::new(Mutex::new(WireProbe {
            ready: true,
            flushed: true,
            ..Default::default()
        }));
        let permits = Arc::new(tokio::sync::Semaphore::new(1));
        let job = tokio::spawn(writer(
            Probe(wire.clone()),
            queue.clone(),
            state.clone(),
            permits.clone().acquire_owned().await.unwrap(),
        ));
        tokio::time::timeout(Duration::from_secs(1), bus.entered.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        queue.peer_closed();
        owner.close();
        let (replacement, _events, _) = state
            .registry
            .on_peer_connected(&snapshot.wallet, &snapshot.session);
        state
            .registry
            .on_peer_disconnected(&snapshot.wallet, &snapshot.session, owner.id);
        // The pending authority is deliberately not released: Close must cancel it.
        tokio::time::timeout(Duration::from_secs(1), job)
            .await
            .unwrap()
            .unwrap();
        bus.release.add_permits(1);
        assert!(wire.lock().bytes.is_empty());
        assert_eq!(wire.lock().flushes, 1);
        assert_eq!(
            state
                .registry
                .peer_link(&snapshot.wallet, &snapshot.session)
                .unwrap()
                .id,
            replacement.id
        );
        assert_eq!(permits.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_completed_send_can_echo_but_failed_or_timed_out_sends_never_salvage() {
        for outcome in ["complete", "failure", "timeout"] {
            let state = state().await;
            let queue = Arc::new(Queue::default());
            queue
                .pending
                .lock()
                .push(frame(None, false))
                .unwrap_or_else(|_| panic!("space"));
            let wire = Arc::new(Mutex::new(WireProbe {
                ready: true,
                flushed: outcome == "complete",
                fail: outcome == "failure",
                ..Default::default()
            }));
            let permits = Arc::new(tokio::sync::Semaphore::new(1));
            let mut job = Box::pin(writer(
                Probe(wire.clone()),
                queue.clone(),
                state.clone(),
                permits.clone().acquire_owned().await.unwrap(),
            ));
            if outcome == "failure" {
                assert!(futures::poll!(&mut job).is_ready());
            } else {
                assert!(futures::poll!(&mut job).is_pending());
                if outcome == "timeout" {
                    tokio::time::advance(WRITE_DEADLINE).await;
                    assert!(futures::poll!(&mut job).is_ready());
                }
            }
            let flushes = wire.lock().flushes;
            queue.peer_closed();
            {
                let mut wire = wire.lock();
                wire.flushed = true;
                wire.fail = false;
            }
            if outcome == "complete" {
                job.await;
            }
            let wire = wire.lock();
            assert_eq!(wire.flushes, flushes + usize::from(outcome == "complete"));
            assert_eq!(
                wire.bytes,
                if outcome == "complete" {
                    b"\x82\x06secret".to_vec()
                } else {
                    vec![0x82, 6]
                }
            );
            assert_eq!(wire.closes, 0);
            assert_eq!(
                state.feed.socket_write_timeout_count(),
                u64::from(outcome == "timeout")
            );
            assert_eq!(permits.available_permits(), 1);
        }
    }
    #[tokio::test]
    async fn queued_pulse_credentials_recheck_source_floor_and_version_after_dequeue() {
        use crate::gatekeeper::admission::cluster::{
            fence::PulseAdmissionStamp, peer_state::ClusterPeerState, subscriber::ClusterSubscriber,
        };
        use crate::gatekeeper::bus::InProcessBus;
        use crate::protocol::pulse::decentraland::pulse::PeerClusterSnapshot;
        use prost::Message as _;
        const WALLET: &str = "0x1111111111111111111111111111111111111111";
        const SESSION: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        for change in [
            "version",
            "source",
            "realm",
            "floor",
            "pass",
            "local-owner",
            "fenced-current",
            "fenced-replaced",
        ] {
            let bus = Arc::new(InProcessBus::new());
            let plan = Arc::new(std::sync::Mutex::new(PeerClusterSnapshot {
                selected_origin: 1,
                selected_identity: Some(1),
                assignment_state: 1,
                wallet: WALLET.into(),
                session: SESSION.into(),
                cluster_id: "current".into(),
                realm: "realm".into(),
                source_incarnation: "source".into(),
                assignment_version: 1,
                token_not_before: Some(0),
                pass: 1,
                pending_takeovers: vec![],
            }));
            let replies = plan.clone();
            bus.respond_to_requests(Arc::new(move |_, _| {
                Ok(replies.lock().unwrap().encode_to_vec())
            }));
            let state = crate::archipelago::test_support::state_over(
                crate::archipelago::Settings::for_tests(),
                bus.clone(),
            )
            .await;
            let subscriber = ClusterSubscriber::new(
                bus.clone(),
                Arc::new(crate::archipelago::test_support::OpenGateway),
                Arc::new(ClusterPeerState::default()),
                Default::default(),
            );
            state
                .gatekeeper
                .attach_island_subscriber_for_tests(&subscriber);
            let (owner, _events) = if change.starts_with("fenced-") {
                state.registry.on_v4_peer_connected_with_realm_epoch(
                    WALLET,
                    SESSION,
                    "connection-one",
                    true,
                    7,
                )
            } else {
                let (owner, events, _) = state.registry.on_peer_connected(WALLET, SESSION);
                (owner, events)
            };
            let queue = Arc::new(Queue::default());
            let mut credential = frame(Some(owner.clone()), true);
            credential.message = Message::Binary(vec![9].into());
            credential.pulse_admission =
                PulseAdmissionStamp::from_snapshot(&plan.lock().unwrap(), 0);
            if change.starts_with("fenced-") {
                credential.fence_bound = true;
                credential.permit = state
                    .registry
                    .control_position_permit(WALLET, SESSION, 7, false);
            }
            {
                let mut pending = queue.pending.lock();
                for _ in 0..QUEUE_CAPACITY - 1 {
                    assert!(pending.push(frame(None, false)).is_ok());
                }
                assert!(pending.push(credential).is_ok());
                pending.done = true;
            }
            match change {
                "version" => plan.lock().unwrap().assignment_version += 1,
                "source" => plan.lock().unwrap().source_incarnation = "next".into(),
                "realm" => plan.lock().unwrap().realm = "next".into(),
                "floor" => plan.lock().unwrap().token_not_before = Some(1),
                "pass" => plan.lock().unwrap().pass += 1,
                "fenced-current" => {}
                "fenced-replaced" => {
                    state.registry.on_v4_peer_connected_with_realm_epoch(
                        WALLET,
                        SESSION,
                        "connection-two",
                        true,
                        8,
                    );
                }
                "local-owner" => {
                    let close = owner.clone();
                    let response = plan.clone();
                    bus.respond_to_requests(Arc::new(move |_, _| {
                        close.close();
                        Ok(response.lock().unwrap().encode_to_vec())
                    }));
                }
                _ => unreachable!(),
            }
            let frames = Arc::new(Mutex::new(vec![]));
            writer(
                Recorder {
                    frames: frames.clone(),
                    flush: true,
                },
                queue,
                state,
                Arc::new(tokio::sync::Semaphore::new(1))
                    .acquire_owned()
                    .await
                    .unwrap(),
            )
            .await;
            let wrote = frames
                .lock()
                .iter()
                .any(|frame| matches!(frame, Message::Binary(bytes) if bytes.as_ref() == [9]));
            assert_eq!(wrote, matches!(change, "pass"), "{change}");
            if !matches!(change, "pass") {
                assert!(owner.needs_assignment() || owner.is_closed());
            }
        }
    }

    fn durable_writer_pg_available() -> bool {
        if std::env::var("CATALYST_TEST_DATABASE_URL")
            .ok()
            .is_none_or(|value| value.trim().is_empty())
        {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "mandatory PostgreSQL in CI"
            );
            eprintln!("local skip: queued durable credentials require CATALYST_TEST_DATABASE_URL");
            return false;
        }
        true
    }
    #[tokio::test]
    async fn durable_queued_credentials_use_positive_pg_generation_without_subscriber_fallback() {
        if !durable_writer_pg_available() {
            return;
        }
        for change in ["unchanged", "generation", "floor", "hazard", "db-closed"] {
            durable_writer_case(change, false).await;
        }
    }
    #[tokio::test]
    async fn durable_queued_configured_credentials_preserve_exact_committed_fence() {
        if !durable_writer_pg_available() {
            return;
        }
        for change in [
            "unchanged",
            "revision",
            "epoch",
            "token",
            "missing-fence",
            "missing-fence-bound",
            "local-owner",
        ] {
            durable_writer_case(change, true).await;
        }
    }
    async fn durable_writer_case(change: &str, fenced: bool) {
        use crate::gatekeeper::admission::cluster::{
            ClusterConfig, fence::PulseAdmissionStamp, safety::SafetyStore,
        };
        use crate::gatekeeper::bus::InProcessBus;
        use crate::interfaces::test_support::{StubDirectory, StubPolicy, StubScenes};
        use crate::protocol::pulse::decentraland::pulse::{PeerClusterSnapshot, PendingTakeover};
        use prost::Message as _;
        let pool = crate::db::scratch::pool(4).await.unwrap();
        let wallet = format!("0x00000000{}", uuid::Uuid::new_v4().simple());
        let session = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let store = SafetyStore::new(pool.clone());
        let safety = store.snapshot(&wallet).await.unwrap();
        assert!(safety.generation > 0);
        let token_fence = fenced.then(|| TokenFence {
            audience: "queued-committed-fence".into(),
            lane: "realm".into(),
            owner_session: session.into(),
            owner_epoch: 7,
            assignment_revision: 3,
            fencing_token: 9,
        });
        if let Some(fence) = token_fence.as_ref() {
            sqlx::query("INSERT INTO social.archipelago_v4_assignments(deployment_audience,owner_address,lane_key,authority_incarnation,assignment_revision,owner_session,owner_epoch,fencing_token) VALUES ($1,$2,'realm','queued-test',3,$3,7,9)").bind(&fence.audience).bind(&wallet).bind(session).execute(&pool).await.unwrap();
        }
        let plan = PeerClusterSnapshot {
            selected_origin: 1,
            selected_identity: Some(1),
            assignment_state: 1,
            wallet: wallet.clone(),
            session: session.into(),
            cluster_id: "current".into(),
            realm: "realm".into(),
            source_incarnation: "durable-writer-test".into(),
            assignment_version: 1,
            token_not_before: Some(0),
            pass: 1,
            pending_takeovers: vec![],
        };
        let mut stamp = PulseAdmissionStamp::from_snapshot(&plan, 0).unwrap();
        stamp.safety_generation = safety.generation;
        stamp.safety_token_floor = safety.token_floor;
        let bus = Arc::new(InProcessBus::new());
        bus.respond_to_requests(Arc::new(move |_, _| Ok(plan.encode_to_vec())));
        let gatekeeper = Arc::new(
            crate::gatekeeper::CommsState::new(
                pool.clone(),
                crate::http::Outbound::for_tests(),
                crate::gatekeeper::Settings::for_tests(),
                None,
                None,
                Arc::new(StubPolicy::default()),
                Arc::new(StubScenes::default()),
                Arc::new(crate::presence::Presence::new()),
                Arc::new(StubDirectory::default()),
            )
            .with_cluster(ClusterConfig {
                nats_url: Some("nats://fixture".into()),
                control_v4_audience: token_fence.as_ref().map(|fence| fence.audience.clone()),
                ..Default::default()
            })
            .unwrap()
            .with_bus(bus),
        );
        assert!(gatekeeper.island_subscriber().upgrade().is_none());
        let jobs = crate::jobs::Jobs::new();
        let state = crate::archipelago::build_state(
            crate::archipelago::Settings::for_tests(),
            gatekeeper,
            &jobs,
        )
        .await
        .unwrap();
        let (owner, _events) = if fenced {
            state.registry.on_v4_peer_connected_with_realm_epoch(
                &wallet,
                session,
                "queued-connection",
                true,
                7,
            )
        } else {
            let (owner, events, _) = state.registry.on_peer_connected(&wallet, session);
            (owner, events)
        };
        let queue = Arc::new(Queue::default());
        let mut credential = frame(Some(owner.clone()), true);
        credential.message = Message::Binary(vec![9].into());
        credential.pulse_admission = Some(stamp);
        if fenced {
            credential.fence_bound = change != "missing-fence-bound";
            credential.permit = state
                .registry
                .control_position_permit(&wallet, session, 7, false);
            if change != "missing-fence" {
                credential.island_token_fence = token_fence.clone();
            }
        }
        {
            let mut pending = queue.pending.lock();
            for _ in 0..QUEUE_CAPACITY - 1 {
                assert!(pending.push(frame(None, false)).is_ok());
            }
            assert!(pending.push(credential).is_ok());
            pending.done = true;
        }
        match change {
            "generation" => {
                sqlx::query("UPDATE social.island_admission_fences SET generation=generation+1 WHERE wallet=$1").bind(&wallet).execute(&pool).await.unwrap();
            }
            "floor" => {
                sqlx::query(
                    "UPDATE social.island_admission_fences SET token_floor=1 WHERE wallet=$1",
                )
                .bind(&wallet)
                .execute(&pool)
                .await
                .unwrap();
            }
            "hazard" => {
                let op = PendingTakeover {
                    takeover_id: "queued".into(),
                    displaced_session: session.into(),
                    displaced_cluster_id: "old".into(),
                    displaced_realm: "realm".into(),
                };
                store
                    .prepare_takeover(&wallet, "other-source", session, &op)
                    .await
                    .unwrap();
                store
                    .begin_takeover(&wallet, "other-source", &op, 0)
                    .await
                    .unwrap()
                    .unwrap();
            }
            "revision" => {
                sqlx::query("UPDATE social.archipelago_v4_assignments SET assignment_revision=assignment_revision+1 WHERE owner_address=$1").bind(&wallet).execute(&pool).await.unwrap();
            }
            "epoch" => {
                sqlx::query("UPDATE social.archipelago_v4_assignments SET owner_epoch=owner_epoch+1 WHERE owner_address=$1").bind(&wallet).execute(&pool).await.unwrap();
            }
            "token" => {
                sqlx::query("UPDATE social.archipelago_v4_assignments SET fencing_token=fencing_token+1 WHERE owner_address=$1").bind(&wallet).execute(&pool).await.unwrap();
            }
            "local-owner" => {
                state.registry.on_v4_peer_connected_with_realm_epoch(
                    &wallet,
                    session,
                    "new-connection",
                    true,
                    8,
                );
            }
            "db-closed" => pool.close().await,
            _ => {}
        }
        let frames = Arc::new(Mutex::new(vec![]));
        writer(
            Recorder {
                frames: frames.clone(),
                flush: true,
            },
            queue,
            state,
            Arc::new(tokio::sync::Semaphore::new(1))
                .acquire_owned()
                .await
                .unwrap(),
        )
        .await;
        let wrote = frames
            .lock()
            .iter()
            .any(|frame| matches!(frame,Message::Binary(bytes) if bytes.as_ref()==[9]));
        assert_eq!(
            wrote,
            change == "unchanged",
            "{change}: original queued stamp must not be relabeled"
        );
        if change != "unchanged" {
            assert!(owner.needs_assignment() || owner.is_closed());
        }
        jobs.shutdown(std::time::Duration::from_secs(1)).await;
    }
    fn frame(owner: Option<PeerLink>, assignment: bool) -> Frame {
        Frame {
            message: Message::Binary(vec![1].into()),
            owner,
            delivery: assignment.then_some((Some(true), true)),
            assignment,
            pulse_admission: None,
            island_token_fence: None,
            permit: None,
            fence_bound: false,
        }
    }
    #[tokio::test]
    async fn final_lane_conflict_survives_owner_cleanup_before_writer_runs() {
        let state = state().await;
        let queue = Arc::new(Queue::default());
        let registry = PeersRegistry::new();
        let (owner, _events, _) = registry.on_peer_connected("a", "s");
        let conflict = Message::Binary(super::super::craft(super::super::kicked_packet()).into());
        {
            let mut pending = queue.pending.lock();
            pending.owner = Some(owner.clone());
            assert!(pending.push(frame(Some(owner.clone()), true)).is_ok());
            pending.done = true;
        }
        // Final-lane recovery uses the same terminal queue path as socket.kick().
        queue.terminal(Terminal::Kick(conflict.clone()));
        // Force cleanup to win the scheduling race rather than relying on platform timing.
        owner.close();
        let frames = Arc::new(Mutex::new(vec![]));
        let permits = Arc::new(tokio::sync::Semaphore::new(1));
        writer(
            Recorder {
                frames: frames.clone(),
                flush: true,
            },
            queue,
            state,
            permits.acquire_owned().await.unwrap(),
        )
        .await;
        let frames = frames.lock();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], conflict);
        assert!(matches!(frames[1], Message::Close(None)));
    }
    #[tokio::test(start_paused = true)]
    async fn sustained_rate_resets_after_quiet_and_requires_ten_continuous_seconds() {
        let mut rate = HeartbeatRate::default();
        let start = Instant::now();
        for _ in 0..21 {
            assert!(!rate.sample(start));
        }
        assert!(
            !rate.sample(start + Duration::from_secs(2)),
            "quiet removes stale excess"
        );
        let mut rate = HeartbeatRate::default();
        for millis in (0..10_400).step_by(20) {
            assert!(!rate.sample(start + Duration::from_millis(millis)));
        }
        // Threshold is reached20 samples after starting; every subsequent rolling window stays excessive.
        assert!(rate.sample(start + Duration::from_millis(10_400)));
    }
    #[tokio::test(start_paused = true)]
    async fn full_queue_replacement_preserves_deadline_and_only_latest_flush_acknowledges() {
        let state = state().await;
        let queue = Arc::new(Queue::default());
        let registry = PeersRegistry::new();
        let (old, _old_events, _) = registry.on_peer_connected("old", "s1");
        let (new, _new_events, _) = registry.on_peer_connected("new", "s2");
        {
            let mut pending = queue.pending.lock();
            assert!(pending.push(frame(Some(old.clone()), true)).is_ok());
            for _ in 1..QUEUE_CAPACITY {
                assert!(pending.push(frame(None, false)).is_ok());
            }
            let at = pending.full_since;
            assert!(pending.push(frame(Some(new.clone()), true)).is_ok());
            assert_eq!(pending.full_since, at);
            assert_eq!(pending.frames.len(), 64);
            assert!(pending.push(frame(None, false)).is_err());
            pending.done = true;
        }
        assert!(old.needs_assignment());
        assert!(new.needs_assignment());
        let frames = Arc::new(Mutex::new(vec![]));
        let permits = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = permits.clone().acquire_owned().await.unwrap();
        writer(
            Recorder {
                frames,
                flush: true,
            },
            queue.clone(),
            state,
            permit,
        )
        .await;
        assert!(old.needs_assignment(), "superseded payload never flushed");
        assert!(!new.needs_assignment());
        assert_eq!(permits.available_permits(), 1);
        assert!(queue.closed.is_cancelled());
    }
    #[tokio::test(start_paused = true)]
    async fn established_kick_preempts_full_queue_restart_and_congestion_close() {
        let state = state().await;
        let queue = Arc::new(Queue::default());
        let registry = PeersRegistry::new();
        let (owner, _events, _) = registry.on_peer_connected("a", "s");
        {
            let mut pending = queue.pending.lock();
            pending.owner = Some(owner.clone());
            for _ in 0..64 {
                assert!(pending.push(frame(None, false)).is_ok());
            }
        }
        owner.send(SocketEvent::Kicked);
        state.restart.cancel();
        queue.terminal(Terminal::Close(1013, String::new()));
        let frames = Arc::new(Mutex::new(vec![]));
        let permits = Arc::new(tokio::sync::Semaphore::new(1));
        writer(
            Recorder {
                frames: frames.clone(),
                flush: true,
            },
            queue,
            state,
            permits.acquire_owned().await.unwrap(),
        )
        .await;
        let frames = frames.lock();
        assert_eq!(frames.len(), 2);
        assert!(matches!(frames[0], Message::Binary(_)));
        assert!(matches!(frames[1], Message::Close(None)));
    }
    #[tokio::test(start_paused = true)]
    async fn partial_flush_failure_never_acknowledges_or_attempts_a_second_frame() {
        let state = state().await;
        let queue = Arc::new(Queue::default());
        let registry = PeersRegistry::new();
        let (owner, _events, _) = registry.on_peer_connected("a", "s");
        {
            let mut pending = queue.pending.lock();
            assert!(pending.push(frame(Some(owner.clone()), true)).is_ok());
            pending.done = true;
        }
        let frames = Arc::new(Mutex::new(vec![]));
        let permits = Arc::new(tokio::sync::Semaphore::new(1));
        let at = Instant::now();
        let permit = permits.clone().acquire_owned().await.unwrap();
        assert_eq!(permits.available_permits(), 0);
        writer(
            Recorder {
                frames: frames.clone(),
                flush: false,
            },
            queue,
            state,
            permit,
        )
        .await;
        assert_eq!(Instant::now() - at, WRITE_DEADLINE);
        assert_eq!(frames.lock().len(), 1);
        assert!(owner.needs_assignment());
        assert_eq!(permits.available_permits(), 1);
    }
    #[tokio::test]
    async fn reply_ids_remain_individual_frames_and_stale_fence_never_acknowledges() {
        use crate::protocol::archipelago::archipelago as pb;
        use prost::Message as _;
        let state = state().await;
        let queue = Arc::new(Queue::default());
        let registry = PeersRegistry::new();
        let (owner, _events, _) = registry.on_peer_connected("a", "s");
        {
            let mut pending = queue.pending.lock();
            for request_sequence in 1..=3 {
                let packet = pb::ServerPacket {
                    request_sequence,
                    message: Some(pb::server_packet::Message::Ack(pb::Ack {
                        applied_revision: request_sequence,
                        ..Default::default()
                    })),
                };
                let mut reply = frame(Some(owner.clone()), false);
                reply.message = Message::Binary(packet.encode_to_vec().into());
                assert!(pending.push(reply).is_ok());
            }
            let mut stale = frame(Some(owner.clone()), true);
            stale.fence_bound = true;
            assert!(pending.push(stale).is_ok());
            pending.done = true;
        }
        let frames = Arc::new(Mutex::new(vec![]));
        let permits = Arc::new(tokio::sync::Semaphore::new(1));
        writer(
            Recorder {
                frames: frames.clone(),
                flush: true,
            },
            queue,
            state,
            permits.acquire_owned().await.unwrap(),
        )
        .await;
        let frames = frames.lock();
        assert_eq!(frames.len(), 4);
        for (index, frame) in frames[..3].iter().enumerate() {
            let Message::Binary(bytes) = frame else {
                panic!("reply expected")
            };
            let reply = pb::ServerPacket::decode(bytes.clone()).unwrap();
            assert_eq!(reply.request_sequence, index as u64 + 1);
            let Some(pb::server_packet::Message::Ack(ack)) = reply.message else {
                panic!("Ack expected")
            };
            assert_eq!(ack.applied_revision, index as u64 + 1);
        }
        assert!(
            owner.needs_assignment(),
            "stale queued assignment was never acknowledged"
        );
    }
    #[tokio::test(start_paused = true)]
    async fn congestion_requires_ten_continuously_full_seconds_and_emits_1013_at_a_frame_boundary()
    {
        let queue = Arc::new(Queue::default());
        {
            let mut pending = queue.pending.lock();
            for _ in 0..64 {
                assert!(pending.push(frame(None, false)).is_ok());
            }
        }
        tokio::time::advance(Duration::from_secs(9)).await;
        {
            let mut pending = queue.pending.lock();
            assert!(!pending.full_expired(Instant::now()));
            assert!(pending.pop().is_some());
            assert_eq!(pending.full_since, None);
            assert!(pending.push(frame(None, false)).is_ok());
        }
        tokio::time::advance(Duration::from_secs(9)).await;
        assert!(!queue.pending.lock().full_expired(Instant::now()));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(queue.pending.lock().full_expired(Instant::now()));
        let state = state().await;
        let frames = Arc::new(Mutex::new(vec![]));
        let permits = Arc::new(tokio::sync::Semaphore::new(1));
        writer(
            Recorder {
                frames: frames.clone(),
                flush: true,
            },
            queue,
            state,
            permits.acquire_owned().await.unwrap(),
        )
        .await;
        let frames = frames.lock();
        assert_eq!(frames.len(), 1);
        assert!(matches!(&frames[0], Message::Close(Some(frame)) if frame.code == 1013));
    }
    #[tokio::test(start_paused = true)]
    async fn shutdown_drain_waits_for_the_actual_writer_termination_without_a_lost_wakeup() {
        let state = state().await;
        state.restart.cancel();
        let queue = Arc::new(Queue::default());
        let frames = Arc::new(Mutex::new(vec![]));
        let permits = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = permits.clone().acquire_owned().await.unwrap();
        let mut writing = Box::pin(writer(
            Recorder {
                frames: frames.clone(),
                flush: false,
            },
            queue.clone(),
            state,
            permit,
        ));
        assert!(futures::poll!(&mut writing).is_pending());
        let mut drain = Box::pin(tokio::time::timeout(
            Duration::from_secs(16),
            permits.clone().acquire_many_owned(1),
        ));
        assert!(futures::poll!(&mut drain).is_pending());
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(futures::poll!(&mut writing).is_pending());
        assert!(futures::poll!(&mut drain).is_pending());
        assert_eq!(
            permits.available_permits(),
            0,
            "reader exit/restart cannot release an active writer's permit"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        writing.await;
        let all = drain
            .await
            .expect("bounded writer termination wakes the registered permit waiter")
            .unwrap();
        assert!(queue.closed.is_cancelled());
        assert_eq!(
            frames.lock().len(),
            1,
            "a partial close is not followed by another frame"
        );
        drop(all);
        assert_eq!(permits.available_permits(), 1);
    }
    fn sample(link: PeerLink, generation: u64, x: f32, realm: &str) -> PositionSample {
        PositionSample {
            generation,
            permit: None,
            owner: Some(V4Owner {
                address: "wallet".into(),
                session: format!("session-{generation}"),
                epoch: generation,
            }),
            link,
            realm: realm.into(),
            position: [x, 0.0, 0.0],
        }
    }
    #[tokio::test(start_paused = true)]
    async fn cancelled_authority_wait_retains_quiet_sample_or_its_latest_replacement_until_commit()
    {
        for replace in [false, true] {
            let registry = PeersRegistry::new();
            let (link, _events, _) = registry.on_peer_connected("wallet", "session");
            let mut pending = Some(sample(link.clone(), 1, 1.0, "first-realm"));
            let mut next = Instant::now();
            let original_deadline = next;
            let records = Arc::new(Mutex::new(vec![]));
            let (_release, authority) = tokio::sync::oneshot::channel::<bool>();
            let mut recv_branch = Box::pin(commit_position(
                &mut pending,
                &mut next,
                |_| async move { authority.await.unwrap() },
                |_| panic!("cancelled authority wait must not apply"),
            ));
            assert!(futures::poll!(&mut recv_branch).is_pending());
            tokio::select! { biased; _ = std::future::ready(()) => {}, _ = &mut recv_branch => panic!("maintenance branch must cancel pending recv validation") }
            drop(recv_branch);
            assert_eq!(pending.as_ref().unwrap().generation, 1);
            assert_eq!(next, original_deadline);
            if replace {
                pending = Some(sample(link.clone(), 2, 3.0, "latest-realm"));
            }
            let expected_generation = if replace { 2 } else { 1 };
            let recording = records.clone();
            commit_position(
                &mut pending,
                &mut next,
                move |captured| async move {
                    assert_eq!(captured.generation, expected_generation);
                    assert_eq!(captured.owner.as_ref().unwrap().epoch, expected_generation);
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    true
                },
                move |captured| {
                    recording.lock().push((
                        captured.generation,
                        captured.realm,
                        captured.position[0],
                        Instant::now(),
                    ));
                    true
                },
            )
            .await;
            assert!(pending.is_none());
            let records = records.lock();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].0, expected_generation);
            assert_eq!(
                records[0].1,
                if replace {
                    "latest-realm"
                } else {
                    "first-realm"
                }
            );
            assert_eq!(records[0].2, if replace { 3.0 } else { 1.0 });
            assert_eq!(
                next,
                records[0].3 + POSITION_INTERVAL,
                "cadence starts at actual application, not authority-check start"
            );
        }
    }
    #[tokio::test(start_paused = true)]
    async fn authority_latency_never_bunches_applications_and_closed_owner_cannot_commit() {
        let registry = PeersRegistry::new();
        let (link, _events, _) = registry.on_peer_connected("wallet", "session");
        let mut pending = Some(sample(link.clone(), 1, 1.0, "realm"));
        let mut next = Instant::now();
        let mut times = vec![];
        commit_position(
            &mut pending,
            &mut next,
            |_| async {
                tokio::time::sleep(Duration::from_millis(400)).await;
                true
            },
            |_| {
                times.push(Instant::now());
                true
            },
        )
        .await;
        pending = Some(sample(link.clone(), 2, 2.0, "realm"));
        commit_position(
            &mut pending,
            &mut next,
            |_| async { true },
            |_| panic!("must wait500ms since actual application"),
        )
        .await;
        assert!(pending.is_some());
        tokio::time::advance(POSITION_INTERVAL).await;
        commit_position(
            &mut pending,
            &mut next,
            |_| async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                true
            },
            |_| {
                times.push(Instant::now());
                true
            },
        )
        .await;
        assert!(times[1].duration_since(times[0]) >= POSITION_INTERVAL);
        tokio::time::advance(POSITION_INTERVAL).await;
        pending = Some(sample(link.clone(), 3, 3.0, "stale-realm"));
        let deadline = next;
        let (release, authority) = tokio::sync::oneshot::channel::<bool>();
        let mut checking = Box::pin(commit_position(
            &mut pending,
            &mut next,
            |_| async { authority.await.unwrap() },
            |_| panic!("closed owner cannot apply"),
        ));
        assert!(futures::poll!(&mut checking).is_pending());
        link.close();
        release.send(true).unwrap();
        checking.await;
        assert!(pending.is_none());
        assert_eq!(
            next, deadline,
            "refused owner never advances application cadence"
        );
    }
}
