//! Pulse Bevy plugin — the shared, platform-agnostic protocol layer.
//!
//! Owns the [`PulseDecoder`] and the driver lifecycle, and pumps the byte boundary: inbound
//! `ServerMessage` bytes are decoded and dispatched; outbound `ClientMessage`s (handshake, teleport,
//! resync today; input later) are encoded onto the driver. The driver itself (native thread / wasm
//! task) is selected at compile time and never seen here.
//!
//! Connect sequence, mirroring the Unity reference client: once the driver reports
//! [`PulseStatus::Connected`] we send a [`pulse::HandshakeRequest`] (a signed auth chain, identical
//! in shape to the platform's `x-identity-*` header dictionary but delivered as protobuf bytes,
//! with the player's last state as its initial state once built); on the server's
//! `HandshakeResponse` we send the first gameplay message, a [`pulse::TeleportRequest`] announcing
//! our realm + position so the server starts streaming same-realm peers — unless the initial state
//! already carried this realm — and then the profile version, which the servers ignore in the
//! handshake. Teleports and emotes leave through a [`DiscreteEventPacer`]: the servers end the
//! session of a peer that overruns the budget they share.
//!
//! Reconnection model: a driver runs exactly one connection attempt and, when that attempt ends,
//! drops its channel ends. Its last status (the server's `Disconnected(reason)`, or a failure of its
//! own), or failing that the pipe close, says the transport is gone; the protocol layer also closes
//! a connection itself when its one handshake fails. Either way it rebuilds the whole driver/link
//! from `Down` when the [`ReconnectPolicy`] says, or parks in `Dead` for a terminal reason. Initial
//! connect is just the first such build.

use std::sync::{Arc, OnceLock};

use alloy_core::primitives::Address;
use bevy::prelude::*;
use bevy::tasks::{IoTaskPool, Task};
use common::{
    bounds_calc::scene_regions,
    structs::{CurrentRealm, EmoteMask, OutOfWorld, PlayerTeleported, PrimaryUser, WorldHold},
    util::{TaskCompat, TaskExt},
};
use dcl_component::proto_components::kernel::comms::rfc4;
use dcl_component::proto_components::pulse;
use dcl_component::transform_and_parent::DclTranslation;
use ipfs::{ActiveEntitiesRequest, EntityDefinition, IpfsAssetServer, IpfsIo};
use multihash_codetable::MultihashDigest;
use prost::Message as _;
use tokio::sync::mpsc;
use wallet::Wallet;

#[path = "debug.rs"]
mod debug;

use super::pacing::DiscreteEventPacer;
use super::reconnect::{Failure, Next, ReconnectPolicy};
use super::transport::{
    self, FrameKind, PulseDriverHandle, PulseFrame, PulseLink, PulseReliability, PulseStatus,
    PulseTransportConfig,
};
use super::{LocalEmote, PulseCtx, PulseDecoder, PulseEvent, PulseParcelGrid};
use crate::global_crdt::{
    CrdtContexts, GlobalCrdtState, NetworkUpdate, PlayerMessage, PlayerUpdate, SceneRealms,
};
use crate::profile::CurrentUserProfile;
use crate::{NetworkMessage, Transport, TransportType};
use bevy::platform::collections::HashMap;

/// Realm to announce verbatim, instead of deriving one from the realm we're in — `--pulse-realm`
/// on a `--server-mode` engine. The orchestrator that spawns a local-preview server already knows
/// the LSD realm key (it mints it), so handing it over saves the server deriving it and works
/// before any scene has loaded. Orchestrated servers don't use this: they host several realms at
/// once and get one per scene on the `add-scene` command.
#[derive(Resource)]
pub struct PulseRealmOverride(pub String);

/// Insert this resource to connect to a Pulse server. Absent → the plugin is fully inert.
#[derive(Resource, Clone)]
pub struct PulseConfig {
    pub transport: PulseTransportConfig,
    pub parcel_grid: PulseParcelGrid,
    /// Identifies this server instance; folded into the handshake connect signature. Empty on dev.
    pub server_id: String,
}

/// The Pulse connection's lifecycle — a single linear progression with three off-ramps (`Down` to
/// rebuild, `Dead` to give up, `Paused` where the user parked it). The driver/link are live from
/// `Connecting` through `Established`; `Paused`, `Down` and `Dead` have no driver. There are no side-flags beyond `PulseSession::accepted`, kept
/// for the reconnect policy: the transport-up signal *is* the `Connecting → Idle` transition, and a
/// disconnect's reason is consumed the moment it arrives.
#[derive(Debug)]
enum Connection {
    /// Explicitly disconnected by the user. Only an explicit resume dials again.
    Paused,
    /// No live driver. The next tick after `respawn_at` (re)builds one. This is both the initial
    /// state and where every retryable failure lands, at the time the [`ReconnectPolicy`] gives.
    Down { respawn_at: f64 },
    /// Driver up, waiting for it to report `Connected` (the ENet/WebTransport connect completing).
    Connecting,
    /// Transport connected, ready to sign once an identity is present.
    Idle,
    /// Signing the auth chain off-thread; resolves to the encoded `ClientMessage(handshake)` bytes.
    /// Re-signed on each attempt so the connect-signature timestamp is fresh when sent.
    Signing(Task<Result<Vec<u8>, String>>),
    /// Handshake sent; awaiting the server's `HandshakeResponse` until `timeout_at`.
    AwaitingResponse { timeout_at: f64 },
    /// Handshake accepted and the realm announced, by its initial state or a teleport; steady state.
    Established,
    /// Terminally disconnected (banned, evicted by a newer session, flagged for misbehaviour, or a
    /// code this build doesn't know) — no reconnect attempted. A realm change while here sets
    /// `Down { respawn_at: 0.0 }` and resets the [`ReconnectPolicy`].
    Dead,
}

/// Cooldown before looking the local scene development realm up again after a failed lookup.
const RETRY_COOLDOWN_SECS: f64 = 2.0;
/// How long the server has to answer the handshake, from the moment it is sent, before the
/// connection is closed as failed. The servers' own pre-auth deadline is 30 s.
const HANDSHAKE_RESPONSE_TIMEOUT_SECS: f64 = 10.0;
/// Minimum spacing between a listener's `SceneListenerUpdate`s. An update rides the server's
/// discrete-event bucket (20/s sustained, 16 burst) and overrunning it is a terminal disconnect,
/// while a busy server's scene set changes many frames in a row — an orchestrator re-adding every
/// scene after an engine restart, a scene reloading. So updates are coalesced: whatever the set is
/// when the interval elapses is what goes out, never one per change.
const AOI_UPDATE_MIN_INTERVAL_SECS: f64 = 1.0;

#[derive(Resource)]
pub(crate) struct PulseSession {
    /// The byte boundary to the current driver. `None` between attempts (`Down`/`Dead`).
    link: Option<PulseLink>,
    /// Current driver; dropping it stops the thread. `None` between attempts. Replaced wholesale on
    /// every (re)connect — this is the "machinery" we reinitialise when the pipe closes.
    _driver: Option<PulseDriverHandle>,
    decoder: PulseDecoder,
    resyncs: [(u64, u32); 60],
    /// What this session is on the wire — see [`PulseRole`]. Everything that differs between a
    /// player and a listening server lives in there.
    role: PulseRole,
    /// The realm announced on a locally served realm: the LSD key derived from the preview scene
    /// entity id (see [`lsd_realm_key`]), so two `dcl start` previews don't share a Pulse partition
    /// even though every dev server advertises the same realm name. Resolved off the dev server
    /// by `resolve_lsd_realm`, which re-announces once it lands; meaningless off a local realm.
    lsd_realm: LsdRealm,
    /// Realm announced verbatim, from [`PulseRealmOverride`]. A server told which realm it hosts
    /// derives nothing: it is up before any scene has loaded, and in a local preview the key is
    /// minted by the orchestrator that spawned it.
    realm_override: Option<String>,
    /// World ↔ parcel mapping, used to build our own `TeleportRequest`.
    grid: PulseParcelGrid,
    /// Where to (re)connect — kept so we can rebuild the driver without the `PulseConfig` resource.
    transport_config: PulseTransportConfig,
    /// Server instance id, folded into the connect signature (re-signed on each attempt).
    server_id: String,
    /// Last `PlayerState` we built, cached by movement's `Broadcast::to_pulse` whether or not
    /// anything is sent. An outbound `EmoteStart` attaches this — the server rejects an emote with
    /// a null `player_state` — and the next handshake carries it. One connection, one outbound
    /// stream, so it lives with the session rather than with any one of the transports feeding it.
    last_state: Option<pulse::PlayerState>,
    /// The emote we last started, cached by the emote's `Broadcast::to_pulse` until it is stopped,
    /// for the next handshake's initial state. Kept like `last_state`: whether or not there is a
    /// connection to send it on.
    local_emote: Option<LocalEmote>,
    /// Teleports and emotes waiting for the servers' event budget. They are offered from the
    /// moment the handshake is being signed ([`Self::offers_paced`]) and let out only while
    /// `Established`, so one made while the handshake is in flight waits here for the acceptance,
    /// and one made before signing starts is never offered: the handshake's initial state carries
    /// the pose instead. A realm change drops a waiting teleport, which names the realm left. A
    /// new driver starts with none: what waited was meant for the last connection.
    pacer: DiscreteEventPacer,
    /// When to connect again after a failure, and when to stop.
    policy: ReconnectPolicy,
    /// Whether the current connection's handshake was accepted.
    accepted: bool,
    /// The realm the pending or accepted handshake's initial state carried. The server holds the
    /// player there, so acceptance needs no teleport into it.
    initial_realm: Option<String>,
    state: Connection,
}

/// Where the LSD realm key for the realm we're on is at — see `resolve_lsd_realm`.
enum LsdRealm {
    /// Nothing in flight; the next tick after `retry_at` on a local realm starts a fetch.
    Unresolved { retry_at: f64 },
    /// Reading the preview scene's entity id off the dev server.
    Fetching(Task<Result<String, String>>),
    /// The key, as announced.
    Resolved(String),
}

/// What a session joined Pulse as. The two roles share a connection and a decoder and almost
/// nothing else: they announce different things at handshake, only one of them may ever send, and
/// inbound state lands somewhere different. One enum rather than a pair of `Option` fields that must
/// never both be set — the role is decided once, at session creation, and never changes.
enum PulseRole {
    /// A player on a realm. Pulse is the realm's avatar-state transport, so everything it receives
    /// feeds the one shared crdt context through one routing transport.
    Player(PlayerRole),
    /// An authoritative server, joined receive-only as a *scene listener* over the parcels it hosts.
    /// It is never a subject, and what it receives is demuxed into the per-scene crdt contexts.
    Listener(ListenerRole),
}

/// A client session's end of the routing.
struct PlayerRole {
    /// The crdt context this session feeds: Pulse is the *realm's* avatar-state transport, so that
    /// is always the shared context.
    context: Entity,
    /// `context`'s update channel, kept only to seed the routing transport's [`PulseSink`] when one
    /// is spawned. Routing never reads it — the component on the transport is what resolves.
    sink: mpsc::Sender<NetworkUpdate>,
    /// The realm's routing `Transport` entity, which doubles as the `transport_id` every inbound
    /// Pulse update is attributed to. `None` off a Pulse realm (the entity is despawned with the
    /// realm's transports): nothing routes then, so peers the decoder still tracks stay out of
    /// the engine until a routing entity exists again.
    ///
    /// Using the real transport entity — rather than a synthetic marker — is what makes presence
    /// work: `ForeignPlayer.transports` holds it, so despawning it on a realm change drops every
    /// Pulse peer from their presence set, exactly as it does for LiveKit and ws-room.
    routing_transport: Option<Entity>,
    /// The realm `routing_transport` was spawned for: `follow_realm` rebuilds the transport only
    /// when this no longer matches the current realm.
    routing_realm: Option<String>,
    /// Set by `follow_realm` on a realm change, cleared by `flush_replay` once it has re-emitted the
    /// decoder's held state for the realm — see [`PulseDecoder::replay`]. A flag rather than an
    /// immediate replay because the routing entity it delivers on is spawned by a deferred command.
    replay_pending: bool,
}

/// A scene listener's view of the world it hosts: which crdt context owns each parcel, the AoI those
/// parcels add up to, and the per-context plumbing inbound state is routed through.
///
/// The AoI is announced in the handshake and reassigned afterwards with `SceneListenerUpdate`, the
/// one post-auth message a listener may send besides `Resync`. The server swaps the set in place, so
/// the connection and identity survive a change — see [`set_listener_aoi`].
#[derive(Default)]
struct ListenerRole {
    /// Realm → hosted parcel → the Pulse `Transport` of the context whose scene covers it. Pulse
    /// reports *where* a peer is; this is what makes that *whose scene* it is in. Keyed by realm as
    /// well as parcel because a parcel index only means something inside one: every world numbers
    /// its parcels from 0,0, so a server cohosting two worlds has two different scenes at `0,0`.
    transport_by_parcel: HashMap<String, HashMap<IVec2, Entity>>,
    /// The AoI we want observed: one entry per hosted realm, its parcels consolidated into one rect
    /// per contiguous rectangular block ([`scene_regions`]). The rects tile each realm's parcel set
    /// exactly — no overlap, no over-coverage — so consolidating them is purely a wire-size win
    /// against the server's Σ-area budget.
    aoi: Vec<pulse::SceneListenerAoi>,
    /// The AoI the server holds: what the handshake carried, then what the last update carried.
    /// `flush_listener_aoi` sends an update whenever `aoi` differs from this, so a change is never
    /// lost — only deferred — however many frames it took to settle.
    announced: Vec<pulse::SceneListenerAoi>,
    /// Earliest time the next update may go out — see [`AOI_UPDATE_MIN_INTERVAL_SECS`].
    next_update_at: f64,
    /// The transport each peer was last routed to, so moving between scenes reports a departure on
    /// the one it left. Pulse gives us positions, not scene membership; this turns the one into the
    /// other. Never pruned: an entry for a despawned transport simply stops resolving, and the
    /// peer's next movement re-places it.
    peer_transport: HashMap<Address, Entity>,
}

/// The crdt context channel a Pulse `Transport` feeds, held by the transport entity itself. Every
/// Pulse transport has one — the realm's routing transport on a client, one per hosted context on a
/// listening server — so a route exists exactly as long as its entity does. Despawning the transport
/// is the whole teardown: the peers it carried lose it from their presence set, and any routing that
/// still points at it simply stops resolving. No map to keep in step with the world.
#[derive(Component)]
struct PulseSink(mpsc::Sender<NetworkUpdate>);

/// Deliver an inbound update on `transport`. A silent no-op once the transport is despawned, which
/// is what makes a stale route harmless rather than a leak.
fn deliver(sinks: &Query<&PulseSink>, transport: Entity, address: Address, message: PlayerMessage) {
    let Ok(sink) = sinks.get(transport) else {
        return;
    };
    let _ = sink.0.try_send(
        PlayerUpdate {
            transport_id: transport,
            message,
            address,
        }
        .into(),
    );
}

/// Report that `address` is no longer reachable on `transport`. Presence is the union of the
/// transports a peer is seen on, so this drops it from that one — and once none is left, the peer is
/// despawned.
fn report_left(sinks: &Query<&PulseSink>, transport: Entity, address: Address) {
    let Ok(sink) = sinks.get(transport) else {
        return;
    };
    let _ = sink.0.try_send(NetworkUpdate::PlayerLeft {
        transport_id: transport,
        address,
    });
}

impl PulseRole {
    /// The transport inbound state for `address` should be delivered on. `None` when nobody should
    /// see it: a client off a Pulse realm (its routing transport is gone, and a straggling packet
    /// must not resurrect a peer whose transport went with it), or a listener that has not placed
    /// this peer in any of its scenes.
    fn route(&self, address: Address) -> Option<Entity> {
        match self {
            Self::Player(player) => player.routing_transport,
            Self::Listener(listener) => listener.peer_transport.get(&address).copied(),
        }
    }

    /// Place `address` in whichever of our scenes covers `parcel` of `realm`, returning the
    /// transport it just left when that moves it — Pulse presence is per context, so the one it left
    /// has to be told. A client places nobody: its realm *is* its area of interest.
    fn place(&mut self, address: Address, realm: &str, parcel: IVec2) -> Option<Entity> {
        let Self::Listener(listener) = self else {
            return None;
        };

        let transport = listener
            .transport_by_parcel
            .get(realm)
            .and_then(|parcels| parcels.get(&parcel))
            .copied();

        let left = match listener.peer_transport.get(&address).copied() {
            Some(previous) if Some(previous) != transport => {
                listener.peer_transport.remove(&address);
                Some(previous)
            }
            _ => None,
        };

        if let Some(transport) = transport {
            listener.peer_transport.insert(address, transport);
        }

        left
    }

    /// Forget `address` entirely, returning the transport to report its departure on.
    fn forget(&mut self, address: Address) -> Option<Entity> {
        match self {
            Self::Player(player) => player.routing_transport,
            Self::Listener(listener) => listener.peer_transport.remove(&address),
        }
    }

    /// Whether there is anything to connect for. A player always has: avatar state rides Pulse on
    /// every realm, so its connection is dialled as soon as there is an identity and kept alive
    /// across realm changes. A listener has only while it has scenes to observe — an empty AoI is
    /// not announceable, and burning a connection on one only earns a rejection.
    fn wants_connection(&self) -> bool {
        match self {
            Self::Player(_) => true,
            Self::Listener(listener) => !listener.aoi.is_empty(),
        }
    }

    /// What this role announces itself as at handshake. `None` when there is nothing to announce
    /// yet (see [`Self::wants_connection`]).
    fn announcement(
        &self,
        profile_version: i32,
        initial_state: Option<pulse::PlayerInitialState>,
    ) -> Option<Announcement> {
        if !self.wants_connection() {
            return None;
        }
        Some(match self {
            Self::Player(_) => Announcement::Player {
                profile_version,
                initial_state,
            },
            Self::Listener(listener) => Announcement::Listener {
                aoi: listener.aoi.clone(),
            },
        })
    }

    fn listener_mut(&mut self) -> Option<&mut ListenerRole> {
        match self {
            Self::Listener(listener) => Some(listener),
            Self::Player(_) => None,
        }
    }

    /// `handshake` / `scene-listener handshake (2 realms, 5 rects)`, for logs.
    fn handshake_label(&self) -> String {
        match self {
            Self::Player(_) => "handshake".to_owned(),
            Self::Listener(listener) => {
                format!("scene-listener handshake ({})", describe_aoi(&listener.aoi))
            }
        }
    }

    fn player_mut(&mut self) -> Option<&mut PlayerRole> {
        match self {
            Self::Player(player) => Some(player),
            Self::Listener(_) => None,
        }
    }
}

/// `2 realms, 5 rects` — the shape of an announced area of interest, for logs.
fn describe_aoi(aoi: &[pulse::SceneListenerAoi]) -> String {
    let rects: usize = aoi.iter().map(|realm| realm.parcel_rects.len()).sum();
    format!("{} realms, {rects} rects", aoi.len())
}

/// A pending handshake's contents, before it is signed. Both variants carry the same auth chain;
/// they differ in what they assert about the peer — a player its profile version and, once it has
/// been somewhere, where it is, a listener the parcels it wants to observe.
enum Announcement {
    Player {
        profile_version: i32,
        initial_state: Option<pulse::PlayerInitialState>,
    },
    Listener {
        aoi: Vec<pulse::SceneListenerAoi>,
    },
}

impl Announcement {
    /// Sign and encode into the `ClientMessage` that opens the session.
    async fn into_message(self, wallet: Wallet, server_id: String) -> Result<Vec<u8>, String> {
        let message = match self {
            Self::Player {
                profile_version,
                initial_state,
            } => pulse::client_message::Message::Handshake(
                build_handshake_request(&wallet, &server_id, profile_version, initial_state)
                    .await?,
            ),
            Self::Listener { aoi } => pulse::client_message::Message::SceneListenerHandshake(
                build_listener_handshake_request(&wallet, &server_id, aoi).await?,
            ),
        };

        Ok(pulse::ClientMessage {
            message: Some(message),
        }
        .encode_to_vec())
    }
}

impl PulseSession {
    /// Watchdog input: authenticated multiplayer health and intentional/terminal suspension.
    pub(crate) fn connectivity(&self) -> (bool, bool) {
        (
            matches!(self.state, Connection::Established),
            matches!(self.state, Connection::Paused | Connection::Dead),
        )
    }

    fn connection_enabled(&self, no_render: bool) -> Result<bool, &'static str> {
        if no_render || common::structs::server_mode() {
            return Err("Only a graphical player client can control the Pulse connection");
        }
        if !matches!(self.role, PulseRole::Player(_)) {
            return Err("Only a player session can control its Pulse connection");
        }
        match self.state {
            Connection::Dead => Err("A terminal disconnect holds until the realm changes"),
            Connection::Paused => Ok(false),
            _ => Ok(true),
        }
    }

    fn set_connection_enabled(
        &mut self,
        enabled: bool,
        no_render: bool,
        commands: &mut Commands,
    ) -> Result<(), &'static str> {
        if self.connection_enabled(no_render)? == enabled {
            return Ok(());
        }
        if enabled {
            // A resume is the user's own fresh start, like a realm change after a terminal stop:
            // it dials at once, and the backoff counts from the first attempt again.
            self.state = Connection::Down { respawn_at: 0.0 };
            self.policy.reset();
        } else {
            if let Some(transport) = self
                .role
                .player_mut()
                .and_then(|p| p.routing_transport.take())
            {
                commands.entity(transport).try_despawn();
            }
            self.link = None;
            self._driver = None;
            self.decoder.reset();
            self.last_state = None;
            self.accepted = false;
            self.state = Connection::Paused;
        }
        Ok(())
    }

    /// Forward an inbound Pulse update for a peer standing in `parcel` of `realm`, re-homing it
    /// first if that lands in a different scene than the one it was last routed to. A peer that has
    /// left every hosted parcel is reported as departed and its state dropped.
    fn forward_at(
        &mut self,
        sinks: &Query<&PulseSink>,
        address: Address,
        realm: &str,
        parcel: IVec2,
        message: PlayerMessage,
    ) {
        if let Some(left) = self.role.place(address, realm, parcel) {
            report_left(sinks, left, address);
        }
        self.forward(sinks, address, message);
    }

    /// Forward an inbound Pulse update to whoever should see it — see [`PulseRole::route`]. Events
    /// that carry no position (emotes, profile versions) ride the placement the peer's last movement
    /// established.
    fn forward(&self, sinks: &Query<&PulseSink>, address: Address, message: PlayerMessage) {
        if let Some(transport) = self.role.route(address) {
            deliver(sinks, transport, address, message);
        }
    }

    /// Report a peer's departure from wherever it currently is, and forget it. Used for a Pulse
    /// `Left` — it fell out of our interest set, or disconnected.
    fn forget(&mut self, sinks: &Query<&PulseSink>, address: Address) {
        if let Some(transport) = self.role.forget(address) {
            report_left(sinks, transport, address);
        }
    }

    /// Whether a teleport or an emote made now is offered to the pacer: a player's, on a live link
    /// whose handshake is being signed, awaits its answer or was accepted. A listener is never a
    /// subject, and the server refuses it every post-auth message but `Resync`. Before signing
    /// starts, while `Connecting` or `Idle`, one is dropped: the handshake's initial state, built
    /// when signing starts, carries the pose and the emote the player has broadcast by then, and a
    /// teleport from before that, let out after the acceptance, would put the player back where
    /// it was.
    fn offers_paced(&self) -> bool {
        matches!(self.role, PulseRole::Player(_))
            && self.link.is_some()
            && matches!(
                self.state,
                Connection::Signing(_)
                    | Connection::AwaitingResponse { .. }
                    | Connection::Established
            )
    }
}

/// The drain end of a Pulse `Transport` entity's channel — its companion, like
/// `WebsocketRoomTransport.receiver`. `drain_pulse_outbox` decodes and bridges what lands here.
#[derive(Component)]
struct PulseOutbox(mpsc::Receiver<NetworkMessage>);

pub struct PulsePlugin;

impl Plugin for PulsePlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(debug::PulseDebugPlugin);
        app.add_event::<PlayerTeleported>();
        app.add_systems(Startup, configure_pulse);
        app.add_systems(
            Update,
            (
                connect_pulse,
                // ahead of `follow_realm`: a realm change clears the derived key here, so the
                // re-announce that `follow_realm` may send can't carry the previous realm's one.
                resolve_lsd_realm,
                follow_realm,
                pump_pulse,
                drain_pulse_outbox,
            )
                .chain()
                // After the realm-change sweep, so the routing entity `follow_realm` spawns for
                // the new realm is never the one that sweep despawns.
                .after(crate::process_realm_change),
        );
        // Only a server listens, so only a server pays for keeping the listener's routing in step
        // with its scenes — a client has no server contexts to route to and would sweep every
        // entity definition it loads for nothing. Latched by the headless binary before the app
        // is built, so it is fixed by the time this runs.
        if common::structs::server_mode() {
            app.add_systems(
                Update,
                update_listener_aoi
                    .after(connect_pulse)
                    .before(resolve_lsd_realm),
            );
        }
        app.add_systems(Update, pulse_teleport_on_local_move);
    }
}

/// The Pulse port when the endpoint names none: native speaks ENet/UDP, wasm has no ENet so it
/// speaks WebTransport.
const DEFAULT_PULSE_PORT: u16 = if cfg!(target_arch = "wasm32") {
    7743
} else {
    7777
};

/// The SHA-256 the wasm WebTransport driver pins the server's certificate to through
/// `serverCertificateHashes`, for a self-signed server (a local or test deployment). Set by the
/// page before the engine starts (src/web.rs `engine_set_pulse_certificate_hash`) — once per
/// instance of the module, so an in-page scene server (deploy/web/engine/headless.js), over its
/// own memory, sets its own. Unset, production's CA-signed certificate is trusted by default;
/// native (ENet) never needs one.
static CERTIFICATE_HASH: OnceLock<[u8; 32]> = OnceLock::new();

/// Pin the Pulse certificate to `digest` for the rest of the process; a later call is ignored.
pub fn set_certificate_hash(digest: [u8; 32]) {
    let _ = CERTIFICATE_HASH.set(digest);
}

/// A certificate's SHA-256 as the server states it: 64 hex digits, or the base64 the Pulse dev
/// server logs for its generated certificate.
pub fn parse_certificate_hash(text: &str) -> Result<[u8; 32], String> {
    use base64::prelude::*;
    let text = text.trim();
    let invalid = || format!("pulse certificate hash {text:?}: must be a SHA-256 in hex or base64");
    let bytes = if text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        (0..64)
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    } else {
        BASE64_STANDARD.decode(text).map_err(|_| invalid())?
    };
    <[u8; 32]>::try_from(bytes).map_err(|_| invalid())
}

/// Insert the [`PulseConfig`] that activates the transport, on clients and servers alike — a server
/// joins as a scene listener rather than a subject (see [`ListenerRole`]). Targets, in order of
/// precedence, the `--pulse-server` / `?pulseServer=` override, the `PULSE_SERVER` env var, then
/// `pulse-server.<base domain>` — each `host` or `host:port`, a missing port being the
/// platform's. Zone and prod are separate Pulse servers with identical realm names, so a zone
/// deployment must be told to use the zone server or its players land among prod's; the realm a
/// client is on is deliberately not used to infer it. The grid is the Decentraland Genesis City
/// `ParcelEncoder` from the server's appsettings ([`PulseParcelGrid::default`]).
fn configure_pulse(mut commands: Commands) {
    use common::base_domain::{service, service_override, with_default_port, Service};
    let (authority, source) = match service_override(Service::PulseServer) {
        Some(authority) => (authority.to_owned(), "--pulse-server"),
        None => match std::env::var("PULSE_SERVER") {
            Ok(authority) => (authority, "PULSE_SERVER"),
            Err(_) => (service(Service::PulseServer), "base domain"),
        },
    };
    let (host, port) = match with_default_port(&authority, DEFAULT_PULSE_PORT) {
        Ok(endpoint) => endpoint,
        Err(e) => {
            warn!("pulse: server endpoint {authority:?} ({source}): {e}");
            return;
        }
    };
    let cert_hash = CERTIFICATE_HASH.get().map(|digest| digest.to_vec());
    info!(
        "pulse: configured for {host}:{port} ({source}){}",
        if cert_hash.is_some() {
            ", certificate pinned"
        } else {
            ""
        }
    );
    commands.insert_resource(PulseConfig {
        transport: PulseTransportConfig {
            host,
            port,
            cert_hash,
        },
        parcel_grid: PulseParcelGrid::default(),
        server_id: String::new(),
    });
}

/// Build a fresh driver + its protocol-side link for `config`.
fn spawn_driver(config: &PulseTransportConfig) -> (PulseLink, PulseDriverHandle) {
    let (link, channels) = transport::pulse_channels(1024);
    let driver = transport::spawn_pulse_driver(config.clone(), channels);
    (link, driver)
}

/// Bring a session up once a [`PulseConfig`] is present. No-op afterwards (session exists). The
/// driver itself isn't spawned here — the session starts in `Down`, and `pump_pulse` builds it on
/// the first tick, so initial connect and reconnect share one path.
fn connect_pulse(
    mut commands: Commands,
    contexts: Res<CrdtContexts>,
    crdt: Query<&GlobalCrdtState>,
    config: Option<Res<PulseConfig>>,
    session: Option<Res<PulseSession>>,
    realm_override: Option<Res<PulseRealmOverride>>,
) {
    let (Some(config), None) = (config, session) else {
        return;
    };

    // client-only (`configure_pulse` bails in server mode): the realm's avatar state feeds the
    // single shared context, which is spawned with the plugin and outlives every session.
    let context = contexts.shared();
    let Ok(crdt) = crdt.get(context) else {
        return;
    };

    let role = if common::structs::server_mode() {
        PulseRole::Listener(ListenerRole::default())
    } else {
        PulseRole::Player(PlayerRole {
            context,
            sink: crdt.get_sender(),
            routing_transport: None,
            routing_realm: None,
            replay_pending: false,
        })
    };

    commands.insert_resource(PulseSession {
        link: None,
        _driver: None,
        decoder: PulseDecoder::new(config.parcel_grid),
        resyncs: [(0, 0); 60],
        role,
        lsd_realm: LsdRealm::Unresolved { retry_at: 0.0 },
        realm_override: realm_override.map(|announced| announced.0.clone()),
        grid: config.parcel_grid,
        transport_config: config.transport.clone(),
        server_id: config.server_id.clone(),
        last_state: None,
        local_emote: None,
        pacer: DiscreteEventPacer::default(),
        policy: ReconnectPolicy::default(),
        accepted: false,
        initial_realm: None,
        state: Connection::Down { respawn_at: 0.0 },
    });

    info!(
        "pulse: session created for {}:{}",
        config.transport.host, config.transport.port
    );
}

/// Keep the routing `Transport` entity in step with the current realm: (re)spawn it on a realm
/// change and — if already connected — announce the new realm with a teleport, or, after a terminal
/// disconnect, connect again. Pulse is the realm's avatar-state transport whatever its byte
/// protocol is, so this follows the realm itself rather than any island or room the realm's comms
/// bring up. A teleport still waiting for the servers' budget names the realm left, so a realm
/// change drops it before anything else is offered.
///
/// `process_realm_change` sweeps every `Transport` on a realm change, ours included, and with it
/// every peer the old entity carried, while the server keeps its view of them for us across our own
/// realm change and will not announce them again on a quick return. So a realm change also flags a
/// replay of the decoder's held state for the realm entered ([`PulseDecoder::replay`]), which
/// `flush_replay` delivers once the new entity is queryable.
fn follow_realm(
    mut commands: Commands,
    session: Option<ResMut<PulseSession>>,
    realm: Res<CurrentRealm>,
    player: Query<(&GlobalTransform, Has<OutOfWorld>), With<PrimaryUser>>,
    routing: Query<(), With<PulseOutbox>>,
) {
    let Some(mut session) = session else {
        // `PulseConfig` absent, or `connect_pulse`'s deferred insert hasn't applied yet.
        return;
    };

    if matches!(session.state, Connection::Paused) {
        return;
    }
    let Some(player_role) = session.role.player_mut() else {
        // A listener has no realm to follow: its transports track the scenes it hosts, not the
        // realm the process is on. `update_listener_aoi` owns them.
        return;
    };
    // Only a realm change sweeps the transports (`process_realm_change` despawns everything
    // `With<Transport>`), so only then is a new routing entity needed. Rebuilding it otherwise
    // would be actively harmful: Pulse peers are held in `ForeignPlayer.transports` by this entity
    // alone, and a player with an empty set is despawned on the spot — so every peer would be
    // despawned and respawned, firing `playerDisconnected`/`onLeaveScene` for people who never
    // left. The realm's island can hop (archipelago) without touching this: a peer's Pulse
    // transport does not change with the island.
    let realm_changed = player_role.routing_realm.as_deref() != Some(realm.address.as_str());
    // Confirmed against the world rather than trusted from the id alone. Nothing sweeps transports
    // while the realm is unchanged, so this particular check cannot race a deferred despawn — and if
    // the entity did go away for some other reason (a wallet change sweeps too), falling through
    // rebuilds it instead of leaving Pulse silently holding a dead id with nothing to send on.
    let routing_alive = player_role
        .routing_transport
        .is_some_and(|entity| routing.contains(entity));
    if !realm_changed && routing_alive {
        return;
    }

    // A realm change: the sweep may have queued our old entity for despawn this same frame, but
    // that is a deferred command we cannot observe yet — so drop the one we own explicitly
    // (idempotent) and spawn fresh, rather than presence-checking and racing the flush.
    if let Some(previous) = player_role.routing_transport.take() {
        if let Ok(mut entity) = commands.get_entity(previous) {
            entity.despawn();
        }
    }

    let (sender, receiver) = mpsc::channel(1000);
    let routing_transport = commands
        .spawn((
            Transport {
                transport_type: TransportType::Pulse,
                sender,
                control: None,
                context: player_role.context,
            },
            PulseOutbox(receiver),
            PulseSink(player_role.sink.clone()),
        ))
        .id();

    let player_role = session.role.player_mut().expect("checked above");
    player_role.routing_transport = Some(routing_transport);
    player_role.routing_realm = Some(realm.address.clone());
    player_role.replay_pending = true;
    // A teleport still waiting names the realm left. Let out on the acceptance of the handshake in
    // flight, or as the bucket refills, it would put the player back there.
    if realm_changed {
        session.pacer.drop_waiting(FrameKind::Teleport);
    }
    // Already up (a later realm) → re-teleport now, unless out of world (position provisional behind
    // the loading screen); the spawn `PlayerTeleported` re-announces realm + position. Otherwise the
    // first handshake's `on_handshake_response` sends the initial teleport once established.
    if matches!(session.state, Connection::Established) && in_world(&player) {
        send_teleport(&mut session, &realm, &player);
    }
    // A terminal stop lasts until the user moves to another realm: a recovery they start, so it
    // never loops. Rebuilding a lost routing entity in the same realm is not one.
    if realm_changed && matches!(session.state, Connection::Dead) {
        info!("pulse: realm changed after a terminal disconnect — reconnecting");
        session.state = Connection::Down { respawn_at: 0.0 };
        session.policy.reset();
    }
}

/// Drain the Pulse routing entity's channel each frame and convert each queued message to a Pulse
/// frame via [`Broadcast::to_pulse`] — movement → `PlayerStateInput`, emote → `EmoteStart`/`EmoteStop`
/// — sending what comes back, except an emote, which goes to the pacer to wait for the servers'
/// event budget with the teleports. Messages with no Pulse form (e.g. byte-only chat/profile that
/// happened onto this transport) yield `None` and are dropped. The conversion happens whatever the
/// state of the connection, because it keeps the session's caches current; only an `Established`
/// session sends, an emote made while the handshake is in flight waits for its acceptance, and one
/// made before signing starts is dropped ([`PulseSession::offers_paced`]). Last of all, what the
/// pacer allows now goes out.
fn drain_pulse_outbox(
    session: Option<ResMut<PulseSession>>,
    time: Res<Time>,
    mut outboxes: Query<&mut PulseOutbox>,
) {
    let Some(session) = session else {
        return;
    };
    let session = session.into_inner();
    let now = time.elapsed_secs_f64();
    // Only a player transmits, and keeps the caches a handshake is built from: a listener is never a
    // subject, and the server refuses every message it could send anyway. Its outboxes are still
    // drained — a transport that silently backs up is worse than one that visibly discards.
    let player = matches!(session.role, PulseRole::Player(_));
    let grid = session.grid;
    for mut outbox in outboxes.iter_mut() {
        while let Ok(message) = outbox.0.try_recv() {
            if !player {
                continue;
            }
            let mut ctx = PulseCtx {
                grid: &grid,
                last_state: &mut session.last_state,
                local_emote: &mut session.local_emote,
                now,
            };
            let Some(frame) = message.message.to_pulse(&mut ctx) else {
                continue;
            };
            if DiscreteEventPacer::paces(frame.kind) {
                // Out at once if the bucket allows, ahead of what was queued after it. The
                // handshake in flight carries what the player was doing when signing started,
                // and the acceptance seeds that: an emote made since waits for it, and one made
                // before is not offered (`offers_paced`).
                if session.offers_paced() {
                    session.pacer.offer(frame);
                    release_paced(session, now);
                }
            } else if let (Some(link), Connection::Established) =
                (session.link.as_mut(), &session.state)
            {
                // Not before: an input is superseded by the next one anyway, and the acceptance
                // announces the profile version.
                link.send(frame);
            }
        }
    }
    release_paced(session, now);
}

/// Send the teleports and emotes the pacer allows at `now`. Only an `Established` connection sends;
/// in any other state they keep waiting.
fn release_paced(session: &mut PulseSession, now: f64) {
    let (Some(link), Connection::Established) = (session.link.as_mut(), &session.state) else {
        return;
    };
    for frame in session.pacer.release(now) {
        link.send(frame);
    }
}

/// Drain status + inbound bytes each frame; advance the connection; decode and dispatch.
#[allow(clippy::too_many_arguments)]
fn pump_pulse(
    session: Option<ResMut<PulseSession>>,
    realm: Res<CurrentRealm>,
    wallet: Res<Wallet>,
    time: Res<Time>,
    player: Query<(&GlobalTransform, Has<OutOfWorld>), With<PrimaryUser>>,
    profile: Option<Res<CurrentUserProfile>>,
    sinks: Query<&PulseSink>,
    world_hold: Option<Res<WorldHold>>,
) {
    let Some(session) = session else {
        return;
    };
    let session = session.into_inner();
    let now = time.elapsed_secs_f64();
    let in_world = in_world(&player);

    // Version to announce in the handshake — our connect-time profile announce on Pulse. 0 until the
    // profile loads; the periodic announce (`profile::mod`) corrects it once available. Like that
    // announce, never an edit still deploying.
    let profile_version = profile
        .and_then(|p| p.peer_profile().map(|p| p.version as i32))
        .unwrap_or(0);

    drain_status(session, &sinks, now);
    drive_connection(
        session,
        &sinks,
        &wallet,
        &realm,
        in_world,
        world_hold.is_some(),
        profile_version,
        now,
    );
    flush_replay(session, &sinks, &realm);
    drain_inbound(
        session,
        &sinks,
        &realm,
        &player,
        in_world,
        profile_version,
        now,
    );
    flush_listener_aoi(session, now);
}

/// Deliver a `PulseDecoder::replay` of the realm just entered, the frame after `follow_realm` flagged
/// it — its routing entity is a deferred spawn, and delivery resolves the `PulseSink` on it. Held
/// while the realm can't be named yet (a local realm's key may still be fetching); a realm change in
/// the meantime re-flags it for the new realm, and nothing is replayed for the one skipped.
fn flush_replay(session: &mut PulseSession, sinks: &Query<&PulseSink>, realm: &CurrentRealm) {
    let Some(player) = session.role.player_mut() else {
        return;
    };
    if !player.replay_pending {
        return;
    }
    match player.routing_transport {
        Some(transport) if sinks.contains(transport) => {}
        Some(_) => return,
        None => {
            player.replay_pending = false;
            return;
        }
    }
    let Some(realm_name) = realm_name(session, realm) else {
        return;
    };
    session
        .role
        .player_mut()
        .expect("checked above")
        .replay_pending = false;

    let events = session.decoder.replay(&realm_name);
    if events.is_empty() {
        return;
    }
    info!(
        "pulse: replaying {} held peer(s) for realm {realm_name}",
        events.len() / 2
    );
    for event in events {
        match event {
            PulseEvent::Joined {
                address,
                profile_version,
                parcel,
                realm,
                ..
            } => {
                session.forward_at(sinks, address, &realm, parcel, PlayerMessage::Joined);
                bridge_profile_version(session, sinks, address, profile_version);
            }
            PulseEvent::Movement {
                address,
                movement,
                realm,
                teleport,
                timestamp,
            } => forward_movement(
                session, sinks, address, &realm, movement, teleport, timestamp,
            ),
            // `replay` emits only the two above
            _ => {}
        }
    }
}

/// Bridge a decoded movement into the shared foreign-player pipeline as its own
/// `PlayerMessage::Movement`, reusing `update_player` / `foreign_dynamics` verbatim. Realm + position
/// is the placement: together they decide which of the server's scenes the peer is in, and so which
/// context sees this and everything after it.
fn forward_movement(
    session: &mut PulseSession,
    sinks: &Query<&PulseSink>,
    address: Address,
    realm: &str,
    movement: Box<rfc4::Movement>,
    teleport: bool,
    timestamp: f64,
) {
    let parcel = session.grid.parcel_coords(Vec3::new(
        movement.position_x,
        movement.position_y,
        movement.position_z,
    ));
    session.forward_at(
        sinks,
        address,
        realm,
        parcel,
        PlayerMessage::Movement {
            movement,
            teleport,
            timestamp,
        },
    )
}

/// Drain the driver's status channel into the connection state machine. `link`'s borrow ends at
/// `try_recv`, so the arms are free to mutate `session`. A `Disconnected`/`Failed` status is the
/// driver signing off, handled as the teardown; a bare channel close means it vanished with no word
/// (e.g. panicked) — the fallback teardown. Either way `lost_connection` nulls the link, ending the
/// loop and making a status-then-close sequence idempotent.
fn drain_status(session: &mut PulseSession, sinks: &Query<&PulseSink>, now: f64) {
    while let Some(status) = session.link.as_mut().map(|link| link.status.try_recv()) {
        match status {
            Ok(PulseStatus::Connecting) => debug!("pulse: connecting"),
            Ok(PulseStatus::Connected) => {
                info!("pulse: connected");
                if matches!(session.state, Connection::Connecting) {
                    session.state = Connection::Idle;
                }
            }
            Ok(PulseStatus::Disconnected(reason)) => {
                warn!("pulse: disconnected ({reason:?})");
                let accepted = session.accepted;
                lost_connection(
                    session,
                    sinks,
                    Failure::Disconnected { reason, accepted },
                    now,
                );
            }
            // A failure the driver reports itself, before or after the connection came up — always
            // retryable.
            Ok(PulseStatus::Failed(error)) => {
                warn!("pulse: failed ({error})");
                lost_connection(session, sinks, Failure::Unreached, now);
            }
            Err(mpsc::error::TryRecvError::Empty) => break,
            Err(mpsc::error::TryRecvError::Disconnected) => {
                lost_connection(session, sinks, Failure::Unreached, now);
                break;
            }
        }
    }
}

/// Advance the connection one step. `Down` (re)builds the driver; `Connecting` waits passively for
/// the transport-up status; signing waits additionally for an identity (`PulseConfig` may be present
/// before login). A connection carries one handshake: a sign error or a response timeout closes it,
/// as a rejection does, and the next attempt is a new connection.
#[allow(clippy::too_many_arguments)]
fn drive_connection(
    session: &mut PulseSession,
    sinks: &Query<&PulseSink>,
    wallet: &Wallet,
    realm: &CurrentRealm,
    in_world: bool,
    world_held: bool,
    profile_version: i32,
    now: f64,
) {
    match &mut session.state {
        // Passive states: `Connecting` waits for the transport-up status; the others are terminal or
        // steady.
        Connection::Paused
        | Connection::Dead
        | Connection::Established
        | Connection::Connecting => {}
        Connection::Down { respawn_at } => {
            // Nothing to dial for without an identity to hand the handshake; the server drops an
            // unauthenticated peer after a timeout, which would otherwise cycle the connection
            // for as long as the login screen is up. Nor while the world is held: the lobby
            // signs in before the account is settled, and the server evicts any other session
            // of a wallet that connects.
            if !session.role.wants_connection()
                || now < *respawn_at
                || wallet.address().is_none()
                || world_held
            {
                return;
            }
            let (link, driver) = spawn_driver(&session.transport_config);
            session.link = Some(link);
            session._driver = Some(driver);
            session.accepted = false;
            // A new peer to the servers, with a full budget of its own; nothing waiting was meant
            // for it.
            session.pacer = DiscreteEventPacer::default();
            session.state = Connection::Connecting;
        }
        Connection::Idle => {
            // Hold off until we have an identity to sign with.
            if wallet.address().is_none() {
                return;
            }
            // Where the player already is, for a handshake that follows a pose broadcast before.
            // The realm is named silently: the teleport after acceptance is the one to warn about
            // it.
            let initial = initial_state(
                session.last_state.as_ref(),
                session.local_emote.as_ref(),
                realm_name(session, realm).as_deref(),
                in_world,
                now,
            );
            // Nothing to announce means nothing to connect for — a listener with no scenes yet
            // would only earn a rejection for an empty AoI.
            let Some(announcement) = session.role.announcement(profile_version, initial) else {
                return;
            };
            // What the handshake carries is what the server will hold once it is accepted; a
            // change that lands while it is in flight goes out as an update afterwards — a
            // listener's AoI as one, a player's realm as a teleport on acceptance.
            if let (Announcement::Listener { aoi }, Some(listener)) =
                (&announcement, session.role.listener_mut())
            {
                listener.announced = aoi.clone();
            }
            if let Announcement::Player { initial_state, .. } = &announcement {
                session.initial_realm = initial_state.as_ref().map(|state| state.realm.clone());
            }
            let wallet = wallet.clone();
            let server_id = session.server_id.clone();
            let task = IoTaskPool::get()
                .spawn_compat(async move { announcement.into_message(wallet, server_id).await });
            session.state = Connection::Signing(task);
        }
        Connection::Signing(task) => {
            if let Some(result) = task.complete() {
                match result {
                    Ok(bytes) => {
                        if let Some(link) = session.link.as_mut() {
                            link.send(PulseFrame {
                                bytes,
                                reliability: PulseReliability::Reliable,
                                kind: FrameKind::Control,
                            });
                        }
                        info!("pulse: {} sent", session.role.handshake_label());
                        session.state = Connection::AwaitingResponse {
                            timeout_at: now + HANDSHAKE_RESPONSE_TIMEOUT_SECS,
                        };
                    }
                    Err(err) => {
                        warn!("pulse: failed to build handshake: {err}");
                        lost_connection(session, sinks, Failure::HandshakeFailed, now);
                    }
                }
            }
        }
        Connection::AwaitingResponse { timeout_at } => {
            if now > *timeout_at {
                warn!("pulse: handshake response timed out");
                lost_connection(session, sinks, Failure::HandshakeFailed, now);
            }
        }
    }
}

/// Decode + route inbound `ServerMessage` bytes. Same borrow trick: `link` is released at `try_recv`,
/// so the body can drive the decoder and event handlers through `session`.
fn drain_inbound(
    session: &mut PulseSession,
    sinks: &Query<&PulseSink>,
    realm: &CurrentRealm,
    player: &Query<(&GlobalTransform, Has<OutOfWorld>), With<PrimaryUser>>,
    in_world: bool,
    profile_version: i32,
    now: f64,
) {
    // A player's realm is its whole area of interest, so a peer the server places anywhere else is
    // not for the engine: a delta still in flight from the realm just left, or a peer teleporting
    // out of this one. The decoder keeps tracking them regardless — that is what a later
    // `replay` on returning to their realm, or the join it synthesizes when they enter ours, is
    // built from. A listener places by realm itself.
    let own_realm = match session.role {
        PulseRole::Player(_) => realm_name(session, realm),
        PulseRole::Listener(_) => None,
    };
    let for_engine = |peer_realm: &str| own_realm.as_deref().is_none_or(|own| own == peer_realm);

    while let Some(bytes) = session.link.as_mut().and_then(PulseLink::receive) {
        let events = match pulse::ServerMessage::decode(bytes.as_slice()) {
            Ok(message) => session.decoder.handle(message),
            Err(err) => {
                warn!("pulse: failed to decode ServerMessage: {err}");
                continue;
            }
        };
        for event in events {
            match event {
                // The handshake ack drives the connect sequence.
                PulseEvent::Connected { success, error } => on_handshake_response(
                    session,
                    sinks,
                    realm,
                    player,
                    in_world,
                    profile_version,
                    now,
                    success,
                    error,
                ),
                PulseEvent::Movement {
                    address,
                    movement,
                    realm,
                    teleport,
                    timestamp,
                } => {
                    if !for_engine(&realm) {
                        continue;
                    }
                    forward_movement(
                        session, sinks, address, &realm, movement, teleport, timestamp,
                    )
                }
                // A sequence gap was detected — ask the server to replay full state, reliably.
                PulseEvent::Resync(request) => {
                    let subject_id = request.subject_id;
                    let message = pulse::ClientMessage {
                        message: Some(pulse::client_message::Message::Resync(request)),
                    };
                    let sent = session.link.as_mut().is_some_and(|link| {
                        link.send(PulseFrame {
                            bytes: message.encode_to_vec(),
                            reliability: PulseReliability::Reliable,
                            kind: FrameKind::Control,
                        })
                    });
                    if sent {
                        let second = now as u64;
                        let bucket = &mut session.resyncs[(second % 60) as usize];
                        if bucket.0 != second {
                            *bucket = (second, 0);
                        }
                        bucket.1 += 1;
                    } else {
                        // Unsent, it would hold back every later request for the subject.
                        session.decoder.resync_not_sent(subject_id);
                    }
                }
                // Emote start/stop are delivered natively (`PlayerMessage::EmoteStart` / `EmoteStop`)
                // rather than as an rfc4 `PlayerEmote`: byte-transport emotes are dropped as
                // duplicates, so the Pulse copy has to be distinguishable from them by variant,
                // exactly as movement is.
                PulseEvent::EmoteStart {
                    address,
                    urn,
                    tick,
                    realm,
                    mask,
                } => {
                    if !for_engine(&realm) {
                        continue;
                    }
                    session.forward(
                        sinks,
                        address,
                        PlayerMessage::EmoteStart {
                            urn,
                            incremental_id: tick,
                            mask: EmoteMask::from_wire(mask),
                        },
                    )
                }
                PulseEvent::EmoteStop {
                    address,
                    completed,
                    realm,
                } => {
                    if !for_engine(&realm) {
                        continue;
                    }
                    session.forward(sinks, address, PlayerMessage::EmoteStop { completed })
                }
                // A peer entered our interest set. Report the arrival, then their initial profile
                // version; the version alone would register presence, but saying so explicitly
                // matches the other transports and doesn't depend on it carrying one.
                PulseEvent::Joined {
                    address,
                    profile_version,
                    parcel,
                    realm,
                    ..
                } => {
                    if !for_engine(&realm) {
                        continue;
                    }
                    session.forward_at(sinks, address, &realm, parcel, PlayerMessage::Joined);
                    bridge_profile_version(session, sinks, address, profile_version);
                }
                // A later announcement. Bridged as an rfc4 `AnnounceProfileVersion` so it reuses the
                // same profile path as the byte transports; the set is idempotent.
                PulseEvent::ProfileVersion {
                    address,
                    version,
                    realm,
                } => {
                    if !for_engine(&realm) {
                        continue;
                    }
                    bridge_profile_version(session, sinks, address, version)
                }
                // The peer left our interest set (or disconnected). Report the departure on the
                // transport that was carrying it: presence is the union of the transports a peer is
                // seen on, so this drops them from the set and — once no transport is left —
                // despawns them. On a listening server that is the Pulse transport of whichever
                // scene context it was last placed in.
                PulseEvent::Left { address } => session.forget(sinks, address),
            }
        }
    }
}

/// Bridge a Pulse-announced profile version into the shared foreign-player pipeline as an rfc4
/// `AnnounceProfileVersion`, reusing the same handling as the LiveKit/websocket profile-version path
/// (`global_crdt` → `ProfileEvent::Version`). The address resolves to (or creates) the foreign player.
fn bridge_profile_version(
    session: &PulseSession,
    sinks: &Query<&PulseSink>,
    address: Address,
    version: i32,
) {
    session.forward(
        sinks,
        address,
        PlayerMessage::PlayerData(rfc4::packet::Message::ProfileVersion(
            rfc4::AnnounceProfileVersion {
                profile_version: version.max(0) as u32,
            },
        )),
    );
}

/// Drop the link (the driver closes with GRACEFUL), report every held peer as left, reset the
/// decoder, and go where the policy says. Idempotent: with no live link it's a no-op, so a
/// `Disconnected` status followed by the pipe close only acts once.
fn lost_connection(
    session: &mut PulseSession,
    sinks: &Query<&PulseSink>,
    failure: Failure,
    now: f64,
) {
    if session.link.is_none() {
        return;
    }
    // The link goes first: its drop is what ends the driver, even mid DNS lookup, so the handle's
    // drop that follows joins the thread promptly.
    session.link = None;
    session._driver = None;

    // Whatever the next connection announces is the whole truth; nothing held from this one is. The
    // peers go now rather than freeze until it does, and anyone who left meanwhile stays gone.
    for wallet in session.decoder.reset() {
        session.forget(sinks, wallet);
    }
    session.accepted = false;
    session.state = match session.policy.next(failure, now) {
        Next::RetryAt(respawn_at) => {
            info!(
                "pulse: connection lost — reconnecting in {:.0} s",
                respawn_at - now
            );
            Connection::Down { respawn_at }
        }
        Next::Dead => {
            // Only a player follows a realm, so only for a player does a realm change end the stop.
            let until = match session.role {
                PulseRole::Player(_) => "the realm changes",
                PulseRole::Listener(_) => "restart",
            };
            warn!("pulse: terminal disconnect — not reconnecting until {until}");
            Connection::Dead
        }
    };
}

/// Handle the server's `HandshakeResponse`. On success, send the first gameplay message — a
/// `TeleportRequest` announcing our realm + position, so the server begins streaming same-realm
/// peers (peers in different realms never see each other) — unless the handshake's initial state
/// has put us in this realm already. Then announce the profile version, once there is one.
#[allow(clippy::too_many_arguments)]
fn on_handshake_response(
    session: &mut PulseSession,
    sinks: &Query<&PulseSink>,
    realm: &CurrentRealm,
    player: &Query<(&GlobalTransform, Has<OutOfWorld>), With<PrimaryUser>>,
    in_world: bool,
    profile_version: i32,
    now: f64,
    success: bool,
    error: Option<String>,
) {
    // Ignore a stray response we're not waiting on (e.g. a duplicate after we've established).
    if !matches!(session.state, Connection::AwaitingResponse { .. }) {
        return;
    }
    if !success {
        warn!("pulse: handshake rejected: {}", error.unwrap_or_default());
        lost_connection(session, sinks, Failure::HandshakeFailed, now);
        return;
    }
    info!("pulse: handshake accepted");
    session.policy.on_accepted();
    session.accepted = true;
    session.state = Connection::Established;
    // A listener has no position to announce and is refused every message but `Resync`; its AoI went
    // out with the handshake.
    if matches!(session.role, PulseRole::Listener(_)) {
        return;
    }
    // Suppress the connect-time teleport while out of world — our position is provisional behind the
    // loading screen. The teleport sent when the player is placed in-world (`PlayerTeleported`, via
    // `pulse_teleport_on_local_move`) announces the real position + realm once we have one. Nor is
    // it needed when the initial state carried this realm: the server has us there already, and
    // the inputs that follow move us on. A handshake without one carried no realm.
    let carried =
        session.initial_realm.is_some() && session.initial_realm == realm_name(session, realm);
    if !in_world {
        debug!("pulse: out of world at handshake, deferring teleport to spawn");
    } else if carried {
        debug!("pulse: the handshake's initial state carried the realm, no teleport");
    } else {
        send_teleport(session, realm, player);
    }
    // A fresh connection has its whole budget, so that teleport goes now, and ahead of the
    // announcement; so does a teleport or an emote the player made while the handshake was in
    // flight.
    release_paced(session, now);
    // The servers ignore the version the handshake carries, so it is announced as a message of its
    // own. Until the profile loads there is none, and the periodic announce (`profile::mod`) takes
    // it from there.
    if profile_version > 0 {
        send_profile_version(session, profile_version);
    }
}

/// Whether the current realm serves scenes off a local `dcl start` dev server, which the preview
/// server signals by listing the project's parcels in its `about`. Those realms all advertise the
/// same realm name, so their Pulse partition is keyed differently — see `resolve_lsd_realm`.
fn is_local_realm(realm: &CurrentRealm) -> bool {
    realm.is_local()
}

/// Keep a listening server's routing in step with the scenes it hosts: which context owns each
/// hosted parcel, a Pulse `Transport` per context, and the AoI the whole lot adds up to.
/// Registered in server mode only — a client has no listener and no server contexts.
#[allow(clippy::too_many_arguments)]
fn update_listener_aoi(
    mut commands: Commands,
    session: Option<ResMut<PulseSession>>,
    definitions: Res<Assets<EntityDefinition>>,
    mut definition_events: EventReader<AssetEvent<EntityDefinition>>,
    crdt_contexts: Res<CrdtContexts>,
    scene_realms: Res<SceneRealms>,
    realm: Res<CurrentRealm>,
    states: Query<&GlobalCrdtState>,
    existing: Query<(Entity, &Transport), With<PulseSink>>,
    mut announced_default: Local<Option<String>>,
) {
    // The whole map is rebuilt from scratch below, so only do it when one of its four inputs moved:
    // the loaded definitions, the scene→context registry, the orchestrator's scene→realm map, or
    // the realm a scene without one of its own falls back to. Drained unconditionally — leaving
    // events unread just means reading a bigger backlog next frame.
    let definitions_changed = definition_events.read().count() > 0;

    let Some(session) = session else {
        return;
    };
    let session = session.into_inner();
    if session.role.listener_mut().is_none() {
        return;
    }
    // The realm for a scene that didn't come with one of its own — the one this process is on. On a
    // local preview that's the LSD key rather than the bare realm name every dev server advertises,
    // so a listener lands in the same partition its clients announce. `None` on an orchestrated
    // engine, which is on no realm at all: it hosts several at once and each scene states its own.
    // Not a resource, so it is compared rather than change-detected: `resolve_lsd_realm` can fill
    // the key in at any time.
    let default_realm = announced_realm(session, &realm);
    let default_changed = *announced_default != default_realm;
    if !definitions_changed
        && !default_changed
        && !crdt_contexts.is_changed()
        && !scene_realms.is_changed()
    {
        return;
    }
    announced_default.clone_from(&default_realm);

    let Some(listener) = session.role.listener_mut() else {
        return;
    };

    // Every loaded entity definition with parcel pointers — which on a server is exactly the scenes
    // it hosts — mapped to the crdt context that scene runs on (one per room on an orchestrated
    // server, the shared one on the standalone local-dev server). Non-scene entities (profiles,
    // wearables) point at addresses and urns, so they fall out of the parse; a scene whose context
    // isn't registered yet simply isn't routable yet, and gets picked up on a later frame.
    let mut context_by_parcel: HashMap<String, HashMap<IVec2, Entity>> = HashMap::default();
    for (_, definition) in definitions.iter() {
        let mut parcels = definition.pointers.iter().filter_map(|pointer| {
            let (x, z) = pointer.split_once(',')?;
            Some(IVec2::new(x.trim().parse().ok()?, z.trim().parse().ok()?))
        });
        let Some(first) = parcels.next() else {
            continue;
        };
        let Some(context) = crdt_contexts.try_for_scene_hash(&definition.id) else {
            continue;
        };
        // A scene whose realm is neither stated nor inherited can't be announced — but it is the
        // only thing that's unroutable, so skip it rather than dropping the whole AoI.
        let Some(scene_realm) =
            scene_realms.for_scene_hash(&definition.id, default_realm.as_deref())
        else {
            continue;
        };
        let realm_parcels = context_by_parcel.entry(scene_realm).or_default();
        for parcel in std::iter::once(first).chain(parcels) {
            realm_parcels.insert(parcel, context);
        }
    }

    // The transports that exist now, by the context each serves — the world is the record, so
    // there is no parallel map to fall out of step with it.
    let mut transport_for: HashMap<Entity, Entity> = existing
        .iter()
        .map(|(transport, served)| (served.context, transport))
        .collect();

    // A context that no longer hosts anything loses its transport, which drops every peer it was
    // carrying (the despawn observer sweeps them out of `ForeignPlayer.transports`) — the same
    // teardown a scene room gets when its room goes away.
    transport_for.retain(|context, transport| {
        let keep = context_by_parcel
            .values()
            .any(|parcels| parcels.values().any(|c| c == context));
        if !keep {
            if let Ok(mut entity) = commands.get_entity(*transport) {
                entity.despawn();
            }
        }
        keep
    });

    for context in context_by_parcel
        .values()
        .flat_map(|parcels| parcels.values().copied())
    {
        if transport_for.contains_key(&context) {
            continue;
        }
        let Ok(state) = states.get(context) else {
            continue;
        };
        // A whole transport, both halves, exactly like a client's: inbound state for this context
        // is attributed to it and delivered down its `PulseSink`, and anything queued on it is
        // drained by `drain_pulse_outbox` — which discards rather than transmits while the session
        // is a listener, since a listener may not send. The connection under all of them is the one
        // session link.
        let (sender, receiver) = mpsc::channel(1000);
        let transport = commands
            .spawn((
                Transport {
                    transport_type: TransportType::Pulse,
                    sender,
                    control: None,
                    context,
                },
                PulseOutbox(receiver),
                PulseSink(state.get_sender()),
            ))
            .id();
        transport_for.insert(context, transport);
    }

    // Routing points at the transports, not at the contexts behind them, so a despawn is the whole
    // teardown: a peer still pointed at a dead one simply stops resolving.
    listener.transport_by_parcel = context_by_parcel
        .into_iter()
        .map(|(realm, parcels)| {
            let parcels = parcels
                .into_iter()
                .filter_map(|(parcel, context)| Some((parcel, *transport_for.get(&context)?)))
                .collect();
            (realm, parcels)
        })
        .collect();

    // One entry per realm, its parcels consolidated into one rect per contiguous rectangular block
    // — blocks from adjacent scenes in the same realm merging is fine, the AoI is only a filter.
    // Sorted so an unchanged AoI compares equal across frames (HashMap iteration order does not).
    let mut aoi: Vec<_> = listener
        .transport_by_parcel
        .iter()
        .map(|(realm, parcels)| pulse::SceneListenerAoi {
            realm: realm.clone(),
            parcel_rects: scene_regions(parcels.keys().copied())
                .into_iter()
                .map(|region| pulse::ParcelRect {
                    min_x: region.min.x,
                    min_z: region.min.y,
                    max_x: region.max.x,
                    max_z: region.max.y,
                })
                .collect(),
        })
        .collect();
    aoi.sort_by(|a, b| a.realm.cmp(&b.realm));

    set_listener_aoi(session, aoi);
}

/// Record the AoI we want observed. Before the handshake it decides what the handshake carries,
/// and whether there is anything to connect for at all; afterwards `flush_listener_aoi` tells the
/// server, on its own schedule.
fn set_listener_aoi(session: &mut PulseSession, aoi: Vec<pulse::SceneListenerAoi>) {
    let Some(listener) = session.role.listener_mut() else {
        return;
    };
    if listener.aoi == aoi {
        return;
    }
    listener.aoi = aoi;
    // An empty set is not announceable — the server rejects a `SceneListenerUpdate` with no rects
    // and disconnects, and there is nothing to connect for either. On a live server it is also
    // nearly always momentary: a scene reload drops the old entity definition a frame before the
    // new one lands. So hold the last announced AoI and wait for the next non-empty one rather
    // than cycling the connection over a reload. `drive_connection` stays idle until there is
    // something to observe (`PulseRole::wants_connection`).
}

/// Bring the server's AoI up to date with ours: one `SceneListenerUpdate` carrying the current set
/// whenever it differs from the announced one, no sooner than [`AOI_UPDATE_MIN_INTERVAL_SECS`]
/// after the last. A run of changes inside one interval collapses into a single update of the
/// final set. The connection stays up and the server swaps the AoI in place, which costs no
/// re-authentication and no gap in the positional stream.
fn flush_listener_aoi(session: &mut PulseSession, now: f64) {
    let (Some(link), Connection::Established) = (session.link.as_mut(), &session.state) else {
        return;
    };
    let Some(listener) = session.role.listener_mut() else {
        return;
    };
    if listener.aoi.is_empty()
        || listener.aoi == listener.announced
        || now < listener.next_update_at
    {
        return;
    }

    listener.announced = listener.aoi.clone();
    listener.next_update_at = now + AOI_UPDATE_MIN_INTERVAL_SECS;
    info!(
        "pulse: scene-listener AoI update sent ({})",
        describe_aoi(&listener.announced)
    );
    let message = pulse::ClientMessage {
        message: Some(pulse::client_message::Message::SceneListenerUpdate(
            pulse::SceneListenerUpdate {
                aoi: listener.announced.clone(),
            },
        )),
    };
    link.send(PulseFrame {
        bytes: message.encode_to_vec(),
        reliability: PulseReliability::Reliable,
        kind: FrameKind::Control,
    });
}

/// The realm string we announce to Pulse — the partition key every participant must derive
/// identically (the server compares realms with an ordinal string match). Empty/missing is rejected
/// by the server. `None` until the realm name is known, and on a locally served realm until a
/// `b64-` addressed scene has loaded.
fn announced_realm(session: &PulseSession, realm: &CurrentRealm) -> Option<String> {
    let name = realm_name(session, realm);
    if name.is_none() && !common::structs::multi_tenant() && !is_local_realm(realm) {
        warn!("pulse: no realm name yet (no peers will be visible)");
    }
    name
}

/// The realm name as `announced_realm` resolves it, silently — for the per-frame inbound filter
/// and the replay hold, which retry rather than complain.
fn realm_name(session: &PulseSession, realm: &CurrentRealm) -> Option<String> {
    if let Some(announced) = session.realm_override.as_ref() {
        return Some(announced.clone());
    }

    // An orchestrated engine is on no realm of its own — `change_realm` leaves the name unset there
    // — and every scene it hosts states its own. Nothing to announce, and nothing missing, so this
    // returns before the warning below.
    if common::structs::multi_tenant() {
        return None;
    }

    if is_local_realm(realm) {
        // Every `dcl start` dev server advertises the same realm name, so a preview announces the
        // LSD key derived from the scene it serves instead — see `resolve_lsd_realm`.
        let LsdRealm::Resolved(key) = &session.lsd_realm else {
            debug!("pulse: local realm without a preview scene id yet; deferring announce");
            return None;
        };
        return Some(key.clone());
    }

    realm
        .config
        .realm_name
        .clone()
        .filter(|name| !name.is_empty())
}

/// Pulse's `FieldValidator` `MaxRealmLength`.
const PULSE_MAX_REALM_LENGTH: usize = 255;
const LSD_REALM_PREFIX: &str = "lsd:";
const LSD_REALM_HASHED_PREFIX: &str = "lsd:sha256:";

/// The Local Scene Development realm key for a preview scene entity id — the same string
/// `@dcl/sdk-commands` (`logic/lsd-realm.ts`) mints and unity-explorer derives, byte for byte.
/// Nothing is exchanged at runtime: every party computes this independently, so an implementation
/// that drifts doesn't error, its peers just never see it. Over the limit the id is hashed rather
/// than truncated for the same reason — over the id *including* its `b64-` prefix, lowercase hex.
fn lsd_realm_key(preview_scene_id: &str) -> String {
    let key = format!("{LSD_REALM_PREFIX}{preview_scene_id}");
    if key.len() <= PULSE_MAX_REALM_LENGTH {
        return key;
    }

    let digest = multihash_codetable::Code::Sha2_256.digest(preview_scene_id.as_bytes());
    let hex: String = digest.digest().iter().map(|b| format!("{b:02x}")).collect();
    format!("{LSD_REALM_HASHED_PREFIX}{hex}")
}

/// Resolve the LSD realm key for the local realm we're on and cache it on the session, then
/// re-announce if we already handshook without it. The key comes from the preview scene's *entity
/// id* — `b64-<base64(`{projectRoot}-{machineId}`)>`, minted by the dev server and served
/// identically to every client, so all of them land in the same partition. A locally computed
/// machine id could not do that (a LAN/QR preview peer would compute its own), and the dev
/// server's port could not either (it moves when the port is taken).
///
/// Which scene, and how it is looked up, follows unity-explorer's `LocalSceneEntityIdSource`
/// exactly: the dev server's `scene.json` names the project's base parcel, and the active entity
/// at that parcel is the scene. Asked of the server rather than picked out of whatever scenes
/// happen to be loaded here, so a workspace serving several projects keys off the same one on
/// every client.
fn resolve_lsd_realm(
    session: Option<ResMut<PulseSession>>,
    realm: Res<CurrentRealm>,
    ipfs: IpfsAssetServer,
    time: Res<Time>,
    player: Query<(&GlobalTransform, Has<OutOfWorld>), With<PrimaryUser>>,
) {
    let Some(session) = session else {
        return;
    };
    let session = session.into_inner();
    if session.realm_override.is_some() {
        return;
    }
    if realm.is_changed() {
        session.lsd_realm = LsdRealm::Unresolved { retry_at: 0.0 };
    }
    if !is_local_realm(&realm) {
        return;
    }

    let now = time.elapsed_secs_f64();
    let preview_scene_id = match &mut session.lsd_realm {
        LsdRealm::Resolved(_) => return,
        LsdRealm::Unresolved { retry_at } if now < *retry_at => return,
        LsdRealm::Unresolved { .. } => {
            let base_url = realm
                .about_url
                .strip_suffix("/about")
                .unwrap_or(&realm.about_url)
                .to_owned();
            session.lsd_realm =
                LsdRealm::Fetching(fetch_preview_scene_id(ipfs.ipfs().clone(), base_url));
            return;
        }
        LsdRealm::Fetching(task) => {
            match task.complete() {
                None => return,
                Some(Ok(id)) => id,
                Some(Err(err)) => {
                    warn!("pulse: failed to resolve the local scene development realm, retrying: {err}");
                    session.lsd_realm = LsdRealm::Unresolved {
                        retry_at: now + RETRY_COOLDOWN_SECS,
                    };
                    return;
                }
            }
        }
    };

    let key = lsd_realm_key(&preview_scene_id);
    info!("pulse: local scene development realm resolved to {key}");
    session.lsd_realm = LsdRealm::Resolved(key);
    // A listener has no position to announce and is refused every message but `Resync`; the realm
    // it just learned reaches the server as an AoI update from `update_listener_aoi` instead.
    if matches!(session.role, PulseRole::Player(_))
        && matches!(session.state, Connection::Established)
        && in_world(&player)
    {
        send_teleport(session, &realm, &player);
        release_paced(session, now);
    }
}

/// The dev server's two-step lookup of the preview scene's entity id, as unity-explorer does it:
/// `GET {realm}/scene.json` for the project's base parcel, then the active entity at that parcel.
fn fetch_preview_scene_id(ipfs: Arc<IpfsIo>, base_url: String) -> Task<Result<String, String>> {
    #[derive(serde::Deserialize)]
    struct SceneJson {
        scene: SceneJsonScene,
    }
    #[derive(serde::Deserialize)]
    struct SceneJsonScene {
        base: String,
    }

    IoTaskPool::get().spawn_compat(async move {
        let url = format!("{base_url}/scene.json");
        let scene_json: SceneJson = ipfs
            .client()
            .get(&url)
            .send()
            .await
            .and_then(|response| response.error_for_status())
            .map_err(|e| format!("{url}: {e}"))?
            .json()
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        let base = scene_json.scene.base;

        let entities = ipfs
            .active_entities(ActiveEntitiesRequest::Pointers(vec![base.clone()]), None)
            .await
            .map_err(|e| format!("active entities at {base}: {e}"))?;
        entities
            .into_iter()
            .next()
            .map(|entity| entity.id)
            .ok_or_else(|| format!("no active entity at base parcel {base}"))
    })
}

/// Whether the local player is placed in the world rather than sitting behind the loading screen on
/// a provisional position. No player at all (a server) counts as in-world: there is nothing
/// provisional about a position that does not exist.
fn in_world(player: &Query<(&GlobalTransform, Has<OutOfWorld>), With<PrimaryUser>>) -> bool {
    !player.single().map(|(_, out)| out).unwrap_or(false)
}

/// Send a `TeleportRequest` announcing our current realm + position, so the server (re)starts
/// streaming same-realm peers (peers in different realms never see each other). The `realm` string is
/// the load-bearing field; a one-frame stale position is corrected by the next movement packet. Sent
/// reliably, as the pacer lets it out, and only where [`send_teleport_at`] offers one; no-op without
/// a realm name. Used both on first handshake and on every later realm change (the server supports
/// same-peer re-teleports).
fn send_teleport(
    session: &mut PulseSession,
    realm: &CurrentRealm,
    player: &Query<(&GlobalTransform, Has<OutOfWorld>), With<PrimaryUser>>,
) {
    let world = player
        .single()
        .map(|(transform, _)| transform.translation())
        .unwrap_or(Vec3::ZERO);
    send_teleport_at(session, realm, world);
}

/// Send a `TeleportRequest` for an explicit Bevy world-space position. Used by [`send_teleport`] (the
/// player's current position) and by [`pulse_teleport_on_local_move`] (an instant move, whose final
/// position is passed directly so it doesn't depend on transform propagation having run this frame).
///
/// The request is offered to the session's pacer, which lets it out when the servers' event budget
/// allows, and drops it for a newer one if that is not yet. Whoever knows the time releases what is
/// due at once ([`release_paced`]); `drain_pulse_outbox` does so every frame. Only a player offers
/// one, on a live link whose handshake is being signed, awaits its answer or was accepted
/// ([`PulseSession::offers_paced`]): a teleport made while the handshake is in flight waits for the
/// acceptance, and one made before signing starts, while `Connecting` or `Idle`, is dropped: the
/// handshake's initial state, built then, carries the pose instead.
fn send_teleport_at(session: &mut PulseSession, realm: &CurrentRealm, world: Vec3) {
    if !session.offers_paced() {
        return;
    }

    let Some(realm_name) = announced_realm(session, realm) else {
        return;
    };

    // Bevy render position → DCL world coords (the `-z` flip), then split into parcel + local —
    // exactly the inverse of how inbound state is decoded.
    let dcl = DclTranslation::from_bevy_translation(world).0;
    let (parcel_index, local) = session
        .grid
        .encode_to_parcel(Vec3::new(dcl[0], dcl[1], dcl[2]));

    let teleport = pulse::TeleportRequest {
        parcel_index,
        position_x: pulse::TeleportRequest::position_x_quantized(local.x),
        position_y: pulse::TeleportRequest::position_y_quantized(local.y),
        position_z: pulse::TeleportRequest::position_z_quantized(local.z),
        realm: realm_name.clone(),
    };
    let message = pulse::ClientMessage {
        message: Some(pulse::client_message::Message::Teleport(teleport)),
    };
    session.pacer.offer(PulseFrame {
        bytes: message.encode_to_vec(),
        reliability: PulseReliability::Reliable,
        kind: FrameKind::Teleport,
    });
    debug!("pulse: teleport queued (realm {realm_name}, parcel {parcel_index})");
}

/// Send a `ProfileVersionAnnouncement` for `version`, reliably: peers fetch the profile when it is
/// newer than the one they hold. No-op without a live link.
fn send_profile_version(session: &mut PulseSession, version: i32) {
    let Some(link) = session.link.as_mut() else {
        return;
    };
    let message = pulse::ClientMessage {
        message: Some(pulse::client_message::Message::ProfileAnnouncement(
            pulse::ProfileVersionAnnouncement { version },
        )),
    };
    link.send(PulseFrame {
        bytes: message.encode_to_vec(),
        reliability: PulseReliability::Reliable,
        kind: FrameKind::Control,
    });
    info!("pulse: profile version {version} announced");
}

/// The local player was instantly repositioned (durationless `move_player_to`, `teleport_player`, a
/// spawn snap). Announce it to the Pulse server as a `TeleportRequest` so peers snap to the new
/// position instead of interpolating across the jump. The event carries the final world position, so
/// this doesn't depend on `GlobalTransform` propagation having run this frame. Only a player
/// announces one, from the moment its handshake is being signed ([`send_teleport_at`]): a teleport
/// made while the handshake is in flight goes out on the acceptance, and one made before signing
/// starts, while `Connecting` or `Idle`, is dropped, the handshake's initial state carrying the pose
/// instead.
fn pulse_teleport_on_local_move(
    session: Option<ResMut<PulseSession>>,
    realm: Res<CurrentRealm>,
    mut events: EventReader<PlayerTeleported>,
) {
    // Coalesce to the latest reposition this frame; earlier ones are superseded.
    let Some(position) = events.read().last().map(|ev| ev.position) else {
        return;
    };
    let Some(mut session) = session else {
        return;
    };
    send_teleport_at(&mut session, &realm, position);
}

/// What a handshake says about where the player already is: the last pose built, the realm it is
/// in and, if an emote is still playing (a loop, or a one-shot within its duration), which one and
/// how long ago it started. `None` out of world, where a position behind the loading screen is
/// provisional, and with no pose to restore or no realm to put it in.
fn initial_state(
    last_state: Option<&pulse::PlayerState>,
    emote: Option<&LocalEmote>,
    realm: Option<&str>,
    in_world: bool,
    now: f64,
) -> Option<pulse::PlayerInitialState> {
    if !in_world {
        return None;
    }
    let mut initial = pulse::PlayerInitialState {
        state: Some(last_state?.clone()),
        realm: realm?.to_owned(),
        ..default()
    };

    if let Some(emote) = emote {
        // A duration of 0 is none: the emote loops.
        let duration_ms = emote.duration_ms.filter(|&duration| duration > 0);
        let elapsed_ms = ((now - emote.started_at) * 1000.0) as u32;
        if duration_ms.is_none_or(|duration| elapsed_ms < duration) {
            initial.emote_id = Some(emote.urn.clone());
            initial.emote_duration_ms = duration_ms;
            initial.emote_start_offset_ms = Some(elapsed_ms).filter(|&elapsed| elapsed > 0);
            initial.emote_mask = emote.mask;
        }
    }
    Some(initial)
}

/// Build a `SceneListenerHandshakeRequest`: the same signed-fetch auth chain as the player handshake
/// (the server authenticates both through one pipeline), plus the announced AoI — the parcels this
/// server hosts, per realm. No initial state — a listener is never a subject.
async fn build_listener_handshake_request(
    wallet: &Wallet,
    server_id: &str,
    aoi: Vec<pulse::SceneListenerAoi>,
) -> Result<pulse::SceneListenerHandshakeRequest, String> {
    Ok(pulse::SceneListenerHandshakeRequest {
        protocol_features: 0,
        auth_chain: build_auth_chain(wallet, server_id).await?,
        aoi,
    })
}

/// Build a `HandshakeRequest`: sign `connect:/{server_id}:{ts}:{}` with the local identity and pack
/// the resulting auth chain into the platform's canonical `x-identity-*` dictionary (JSON object,
/// every value a string), serialized as UTF-8 bytes — identical in shape to the HTTP signed-fetch
/// headers, just carried in a protobuf `bytes` field. Mirrors Unity's `BuildAuthChain`.
/// `initial_state` is where the player already is, when it has been anywhere (see [`initial_state`]).
async fn build_handshake_request(
    wallet: &Wallet,
    server_id: &str,
    profile_version: i32,
    initial_state: Option<pulse::PlayerInitialState>,
) -> Result<pulse::HandshakeRequest, String> {
    Ok(pulse::HandshakeRequest {
        protocol_features: 0,
        auth_chain: build_auth_chain(wallet, server_id).await?,
        profile_version,
        initial_state,
    })
}

/// The signed-fetch auth chain both handshakes carry: sign `connect:/{server_id}:{ts}:{}` with the
/// local identity and pack the resulting chain into the platform's canonical `x-identity-*`
/// dictionary (JSON object, every value a string) as UTF-8 bytes — identical in shape to the HTTP
/// signed-fetch headers, just carried in a protobuf `bytes` field. Mirrors Unity's `BuildAuthChain`.
async fn build_auth_chain(wallet: &Wallet, server_id: &str) -> Result<Vec<u8>, String> {
    let timestamp = web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis();

    // NB: not lowercased — the server verifies the signature over this exact string, and Unity
    // signs it verbatim.
    let payload = format!("connect:/{server_id}:{timestamp}:{{}}");
    let auth_chain = wallet
        .sign_message(payload)
        .await
        .map_err(|e| e.to_string())?;

    let mut dict = serde_json::Map::new();
    for (key, value) in auth_chain.headers() {
        dict.insert(key, serde_json::Value::String(value));
    }
    dict.insert(
        "x-identity-timestamp".to_owned(),
        serde_json::Value::String(timestamp.to_string()),
    );
    dict.insert(
        "x-identity-metadata".to_owned(),
        serde_json::Value::String("{}".to_owned()),
    );
    serde_json::to_vec(&dict).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        configure_pulse, lsd_realm_key, parse_certificate_hash, PulseConfig, DEFAULT_PULSE_PORT,
    };
    use bevy::prelude::*;

    /// Hex in either case, or the base64 the Pulse dev server logs — the same digest; anything
    /// that is not exactly 32 bytes is refused.
    #[test]
    fn certificate_hash_is_a_sha256_in_hex_or_base64() {
        let hex = "391d5e8c087abc6e76bc6f76a3aa79cd199b66949c3a0466e9d6526790aceb9e";
        let digest = parse_certificate_hash(hex).unwrap();
        assert_eq!(digest[..3], [0x39, 0x1d, 0x5e]);
        assert_eq!(digest[31], 0x9e);
        assert_eq!(parse_certificate_hash(&hex.to_uppercase()), Ok(digest));
        assert_eq!(
            parse_certificate_hash(" OR1ejAh6vG52vG92o6p5zRmbZpScOgRm6dZSZ5Cs654= "),
            Ok(digest)
        );
        for bad in [
            "",
            "ab",
            &hex[..62],
            &format!("{hex}00"),
            &"zz".repeat(32),
            &"+a".repeat(32),
            "OR1ejAh6vG52vG92o6p5zRmbZpScOgRm6dZSZ5Cs6w==",
            "OR1ejAh6vG52vG92o6p5zRmbZpScOgRm6dZSZ5Cs654",
        ] {
            assert!(parse_certificate_hash(bad).is_err(), "`{bad}`");
        }
    }

    /// The endpoint `configure_pulse` settles on for a given env var (and whatever override is
    /// latched).
    fn configured_endpoint(env: Option<&str>) -> String {
        match env {
            Some(env) => std::env::set_var("PULSE_SERVER", env),
            None => std::env::remove_var("PULSE_SERVER"),
        }
        let mut app = App::new();
        app.add_systems(Startup, configure_pulse);
        app.update();
        let config = app.world().resource::<PulseConfig>();
        format!("{}:{}", config.transport.host, config.transport.port)
    }

    /// The env var beats the base domain, the `--pulse-server` override beats both, and a bare
    /// host takes the platform port. One test rather than several: the env var is process-wide
    /// and the override latches once.
    #[test]
    fn endpoint_precedence() {
        assert_eq!(
            configured_endpoint(None),
            format!("pulse-server.decentraland.org:{DEFAULT_PULSE_PORT}")
        );
        assert_eq!(configured_endpoint(Some("env:2")), "env:2");
        assert_eq!(
            configured_endpoint(Some("env")),
            format!("env:{DEFAULT_PULSE_PORT}")
        );
        common::base_domain::set_services([(
            common::base_domain::Service::PulseServer,
            "startup:1",
        )])
        .unwrap();
        assert_eq!(configured_endpoint(Some("env:2")), "startup:1");
        std::env::remove_var("PULSE_SERVER");
    }

    /// The worked examples published in js-sdk-toolchain's `docs/lsd-identity-and-pulse-realm.md`
    /// (`machineId = "dev-box"`). No party ever sends this key, so drifting from the CLI that mints
    /// it and the other explorers that derive it is silent — hence pinning it here.
    #[test]
    fn matches_the_published_vectors() {
        // project root `/home/dev/my-scene`
        assert_eq!(
            lsd_realm_key("b64-L2hvbWUvZGV2L215LXNjZW5lLWRldi1ib3g="),
            "lsd:b64-L2hvbWUvZGV2L215LXNjZW5lLWRldi1ib3g="
        );

        // project root `/home/dev/` + 200 `a`s — 300 characters raw, so it collapses
        assert_eq!(
            lsd_realm_key(
                "b64-L2hvbWUvZGV2L2FhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhLWRldi1ib3g="
            ),
            "lsd:sha256:783635fb50eadaed0300d80104920bfc55894d5ad2ab69ab6b48c6ff1ddb9da5"
        );
    }

    /// Hashed strictly past `MaxRealmLength`, never at it — a boundary the server would enforce as
    /// a disconnect and the other implementations only as invisibility.
    #[test]
    fn collapses_only_past_the_limit() {
        let at_the_limit = format!("b64-{}", "a".repeat(247));
        assert_eq!(lsd_realm_key(&at_the_limit).len(), 255);
        assert!(lsd_realm_key(&at_the_limit).ends_with(&at_the_limit));

        let over = format!("b64-{}", "a".repeat(248));
        assert_eq!(lsd_realm_key(&over).len(), 75);
        assert!(lsd_realm_key(&over).starts_with("lsd:sha256:"));
    }
}

#[cfg(test)]
mod connection_tests;
