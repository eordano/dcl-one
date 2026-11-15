//! The comms gatekeeper (the old `catalyrst-comms`, upstream
//! comms-gatekeeper), a service inside `catalyst serve`, through local calls
//! and shared SQL over the ONE pool. Seven submodules: `admission` (LiveKit
//! admission for scenes and worlds, the world mints, the Pulse relay
//! authority and the island feed), `moderation` (scene admins and bans,
//! platform bans and warnings), `voice` (private-message and
//! voice tokens, community voice rooms), `streaming` (a place's RTMP ingress
//! credential), `webhook` (the LiveKit webhook that drives presence),
//! `events` (the participant events handed to worlds) and `revocation` (the
//! one entry point that evicts revoked participants from the live rooms).
//!
//! What it needs from the crates above it comes in through
//! [`crate::interfaces`]: world access, the protected wallets of a world
//! place and scene ownership from the worlds permission core
//! ([`AccessPolicy`], worlds' `Policy`, ONE instance shared with the worlds
//! routes and the scene host, where the old gatekeeper fetched the worlds
//! service's `/permissions` document over HTTP), and a world's scenes and
//! the world mints' rooms from the ONE room resolver ([`SceneIdentity`],
//! worlds' `rooms::SceneResolver`). What it offers them: the webhook's
//! participant events ([`events::RoomEventSink`], which worlds' core
//! implements), the one revocation entry point ([`CommsState::revoke`],
//! onto which worlds' core maps its permission and block revocations), the
//! social RPC's voice runtime ([`voice::rooms::VoiceRooms`],
//! implemented by [`CommsState`]) and a scene's live participants
//! (`explore::places::live::Participants`). Social's MLS delivery service
//! (`social::messaging`, on its own state) is mounted beside these routes
//! with its own signed-fetch and community-binding authorization.
//!
//! External runtimes stay separate processes: LiveKit's media server is
//! reached over the network (token minting works offline against the
//! configured key/secret; anything that calls its API degrades to a logged
//! best-effort side effect or an explicit 503), the LAND operators come from
//! a configured upstream through the guarded client, and nothing here can
//! take the process down. The Pulse application relay's room authority
//! (`admission::relay_authority`, `POST /internal/pulse/room-authority/v1`)
//! answers over the same permission core with the frames and the claim and
//! proof types of `protocol::room_authority`. `/scene-stream-access`
//! mints a place's RTMP ingress credential (`streaming`); `/cast/*` answers
//! the old service's 501.
//!
//! The island feed (`admission::cluster`: Pulse's cluster changes turned
//! into island credentials, the realm-lane fence of configured legacy owners) backs
//! `/island-refresh` (`admission::island_refresh`) and the relay authority's
//! island rooms. It arrives over `bus`, `serve`'s one NATS link, which the
//! state owns and the `/ws` connector shares ([`CommsState::bus`]). The
//! connector is part of the gatekeeper's process: it announces each session
//! by call ([`CommsState::announce_session`]) and the island subscriber hands
//! its sockets their islands by call (`admission::cluster::sockets`); neither
//! crosses the broker. The participant removal of the connector's kicks is
//! this state's too ([`CommsState::kick_island_participant`],
//! `admission::island_kick`), so the LiveKit secret has one holder and every
//! island token comes from the island feed. Not carried: the Prometheus
//! `/metrics` route (the cluster series record into the `metrics` facade).

pub mod admission;
pub mod auth;
pub mod bus;
pub mod errors;
pub mod events;
pub use crate::shared::livekit;
pub mod moderation;
pub(crate) mod removal;
pub mod revocation;
pub mod streaming;
pub mod voice;
pub mod webhook;

#[cfg(test)]
mod tests;

use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::{Context, anyhow};
use axum::Router;
use axum::routing::{delete, get, patch, post};
use sqlx::PgPool;

use crate::Address;
use crate::auth::eip1654::Eip1654Validator;
use crate::http::Outbound;
use crate::interfaces::{AccessPolicy, SceneIdentity};
use crate::jobs::{Jobs, PeriodicCfg};
use crate::shared::cors;
use crate::shared::nul_guard;
use events::RoomEventSink;

pub use errors::ApiError;
pub use moderation::store::Moderation;
pub use revocation::RoomRevocation;
pub use voice::db::{VoiceDb, VoiceDbConfig};

/// The raw settings as `Config` (or the process environment) carries them,
/// before resolution. `Settings::resolve` turns them into the checked shape.
#[derive(Debug, Clone, Default)]
pub struct RawSettings {
    /// `LIVEKIT_HOST`: both the signaling URL and the API base derive from it.
    pub livekit_host: String,
    /// `LIVEKIT_API_HOST`: the API base this process calls when it differs.
    pub livekit_api_host: Option<String>,
    /// `LIVEKIT_WS_URL`: the client-facing signaling URL when it differs.
    pub livekit_ws_url: Option<String>,
    pub livekit_api_key: String,
    pub livekit_api_secret: String,
    /// `LIVEKIT_ALLOW_DEV_CREDS`: boot on devkey/devsecret (tokens parse
    /// locally, no real SFU accepts them).
    pub livekit_allow_dev_creds: bool,
    /// `PRIVATE_MESSAGES_ROOM_ID` (default `private-messages`).
    pub private_messages_room_id: String,
    /// `AUTHORITATIVE_SERVER_ADDRESS`: the one wallet that may mint the host
    /// token; unset refuses every `/get-server-scene-adapter` caller.
    pub authoritative_server_address: Option<String>,
    /// `MODERATOR_TOKEN`: the bearer of the user-moderation routes.
    pub moderator_token: Option<String>,
    /// `PLATFORM_USER_MODERATORS`: comma/space-separated moderator wallets.
    pub moderator_addresses: String,
    /// `COMMS_GATEKEEPER_AUTH_TOKEN`: the platform service token in front of
    /// the voice routes and the world ban-status route; unset answers 503.
    pub gatekeeper_auth_token: Option<String>,
    /// `COMMS_LAND_OPERATORS_URL`: a lambdas base whose
    /// `/parcels/{x}/{y}/operators` protects LAND owners/operators from scene
    /// bans (the old `LAMBDAS_URL`); unset protects none (the old degraded
    /// mode without a places database).
    pub land_operators_url: Option<String>,
    /// `LEASE_AUTHORIZATIONS_URL`: the linker-server lease document; unset
    /// lists no lease holder.
    pub lease_authorizations_url: Option<String>,
    /// `VOICE_CHAT_CONNECTION_INTERRUPTED_TTL` (ms, default 300000).
    pub voice_connection_interrupted_ttl_ms: i64,
    /// `VOICE_CHAT_INITIAL_CONNECTION_TTL` (ms, default 300000).
    pub voice_initial_connection_ttl_ms: i64,
    /// `COMMUNITY_VOICE_CHAT_NO_MODERATOR_TTL` (ms, default 300000).
    pub community_voice_no_moderator_ttl_ms: i64,
    /// `VOICE_CHAT_EXPIRED_BATCH_SIZE` (default 50).
    pub voice_expired_batch_size: i64,
    /// `COMMIT_HASH` for `/status`.
    pub commit_hash: Option<String>,
}

impl RawSettings {
    /// The old binary's defaults for everything but the credentials.
    pub fn defaults() -> Self {
        Self {
            livekit_host: "livekit.local".to_string(),
            private_messages_room_id: "private-messages".to_string(),
            voice_connection_interrupted_ttl_ms: 300_000,
            voice_initial_connection_ttl_ms: 300_000,
            community_voice_no_moderator_ttl_ms: 300_000,
            voice_expired_batch_size: 50,
            ..Default::default()
        }
    }
}

pub use crate::shared::livekit::LiveKit;

#[derive(Clone)]
pub struct Settings {
    pub livekit: LiveKit,
    pub private_messages_room_id: String,
    pub authoritative_server_address: Option<Address>,
    pub moderator_token: Option<String>,
    pub moderator_addresses: Vec<Address>,
    pub gatekeeper_auth_token: Option<String>,
    /// Read by `serve` into the permission core's `LandOperatorsReader`
    /// (the ONE reader comms and the storage routes share).
    pub land_operators_url: Option<String>,
    /// The lease document [`admission::scene::get_lease_holders_for_parcels`] reads;
    /// `None` lists no lease holder and fetches nothing.
    pub lease_authorizations_url: Option<String>,
    pub voice: VoiceDbConfig,
    pub commit_hash: Option<String>,
}

/// Masks the moderator and gatekeeper tokens (the LiveKit secret is masked
/// by [`LiveKit`]'s own `Debug`).
impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("livekit", &self.livekit)
            .field("private_messages_room_id", &self.private_messages_room_id)
            .field(
                "authoritative_server_address",
                &self.authoritative_server_address,
            )
            .field(
                "moderator_token",
                &self.moderator_token.as_ref().map(|_| "<set>"),
            )
            .field("moderator_addresses", &self.moderator_addresses)
            .field(
                "gatekeeper_auth_token",
                &self.gatekeeper_auth_token.as_ref().map(|_| "<set>"),
            )
            .field("land_operators_url", &self.land_operators_url)
            .field("lease_authorizations_url", &self.lease_authorizations_url)
            .field("voice", &self.voice)
            .field("commit_hash", &self.commit_hash)
            .finish()
    }
}

fn parse_moderator_addresses(raw: &str) -> Vec<Address> {
    raw.split([',', ' ', '\n'])
        .filter_map(|s| Address::parse(s).ok())
        .collect()
}

/// A configured upstream: http(s) and, so the guarded client can reach it, a
/// public host (a loopback test stub is admitted by the client's own
/// `allowing_private_hosts`, never by this check).
fn upstream_url(var: &str, raw: Option<&str>) -> anyhow::Result<Option<String>> {
    let Some(raw) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let trimmed = raw.trim_end_matches('/');
    let parsed = url::Url::parse(trimmed).with_context(|| format!("{var} is not a URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(anyhow!("{var} must be an http(s) URL"));
    }
    Ok(Some(trimmed.to_string()))
}

impl Settings {
    /// Checks the credentials the way the old binary did at startup: unset or
    /// placeholder LiveKit credentials refuse to start unless the dev opt-in
    /// is set; a non-wallet authoritative address is refused.
    pub fn resolve(raw: RawSettings) -> anyhow::Result<Self> {
        let (api_key, api_secret, configured) = match livekit::resolve_creds(
            raw.livekit_api_key.clone(),
            raw.livekit_api_secret.clone(),
            raw.livekit_allow_dev_creds,
        ) {
            livekit::ResolvedCreds::Configured {
                api_key,
                api_secret,
            } => (api_key, api_secret, true),
            livekit::ResolvedCreds::DevFallback => {
                tracing::warn!(
                    "LIVEKIT_API_KEY / LIVEKIT_API_SECRET unset or still the devkey/devsecret placeholders; \
                         running on the dev defaults: tokens parse locally but no real LiveKit cluster accepts them"
                );
                (
                    livekit::DEV_API_KEY.to_string(),
                    livekit::DEV_API_SECRET.to_string(),
                    false,
                )
            }
            livekit::ResolvedCreds::Unconfigured => {
                return Err(anyhow!(
                    "LIVEKIT_API_KEY / LIVEKIT_API_SECRET are unset or still the devkey/devsecret placeholders; \
                         set real credentials, or set LIVEKIT_ALLOW_DEV_CREDS=true to run with the dev defaults"
                ));
            }
        };
        let endpoints = livekit::resolve_endpoints(
            &raw.livekit_host,
            raw.livekit_api_host.as_deref(),
            raw.livekit_ws_url.as_deref(),
        );
        let authoritative_server_address = match raw
            .authoritative_server_address
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(s) => {
                Some(Address::parse(s).map_err(|e| anyhow!("AUTHORITATIVE_SERVER_ADDRESS: {e}"))?)
            }
            None => None,
        };
        let moderator_token = raw.moderator_token.filter(|s| !s.is_empty());
        let moderator_addresses = parse_moderator_addresses(&raw.moderator_addresses);
        let gatekeeper_auth_token = raw.gatekeeper_auth_token.filter(|s| !s.is_empty());
        if authoritative_server_address.is_none() {
            tracing::warn!(
                "AUTHORITATIVE_SERVER_ADDRESS unset; POST /get-server-scene-adapter rejects every caller"
            );
        }
        if moderator_token.is_none() && moderator_addresses.is_empty() {
            tracing::warn!(
                "neither MODERATOR_TOKEN nor PLATFORM_USER_MODERATORS set; the user-moderation routes reject every caller"
            );
        }
        if gatekeeper_auth_token.is_none() {
            tracing::warn!(
                "COMMS_GATEKEEPER_AUTH_TOKEN unset; every bearer-gated route (voice, private-messages privacy, world ban status) answers 503"
            );
        }
        let land_operators_url = upstream_url(
            "COMMS_LAND_OPERATORS_URL",
            raw.land_operators_url.as_deref(),
        )?;
        let lease_authorizations_url = upstream_url(
            "LEASE_AUTHORIZATIONS_URL",
            raw.lease_authorizations_url.as_deref(),
        )?;
        Ok(Self {
            livekit: LiveKit {
                api_key,
                api_secret,
                client_url: endpoints.client_url,
                api_url: endpoints.api_url,
                configured,
            },
            private_messages_room_id: if raw.private_messages_room_id.is_empty() {
                "private-messages".to_string()
            } else {
                raw.private_messages_room_id
            },
            authoritative_server_address,
            moderator_token,
            moderator_addresses,
            gatekeeper_auth_token,
            land_operators_url,
            lease_authorizations_url,
            voice: VoiceDbConfig {
                connection_interrupted_ttl_ms: raw.voice_connection_interrupted_ttl_ms,
                initial_connection_ttl_ms: raw.voice_initial_connection_ttl_ms,
                community_no_moderator_ttl_ms: raw.community_voice_no_moderator_ttl_ms,
                expired_batch_size: raw.voice_expired_batch_size,
            },
            commit_hash: raw.commit_hash,
        })
    }

    /// Dev credentials, LiveKit "configured" so the room-call paths run
    /// against a stub, every gate unset unless a test sets it.
    pub fn for_tests() -> Self {
        Self {
            livekit: LiveKit {
                api_key: livekit::DEV_API_KEY.to_string(),
                api_secret: livekit::DEV_API_SECRET.to_string(),
                client_url: "wss://livekit.local".to_string(),
                api_url: "http://127.0.0.1:1".to_string(),
                configured: true,
            },
            private_messages_room_id: "private-messages".to_string(),
            authoritative_server_address: None,
            moderator_token: None,
            moderator_addresses: Vec::new(),
            gatekeeper_auth_token: None,
            land_operators_url: None,
            lease_authorizations_url: None,
            voice: VoiceDbConfig::default(),
            commit_hash: None,
        }
    }
}

pub struct CommsState {
    pub deny_list: Arc<crate::deny_list::DenyList>,
    pub pool: PgPool,
    /// The guarded client: the LAND operators and the lease authorizations
    /// (user-influenced URLs).
    pub http: Outbound,
    pub settings: Settings,
    pub livekit: LiveKit,
    pub moderation: Moderation,
    pub bans: Arc<dyn moderation::BanStatus>,
    pub social: Arc<dyn crate::interfaces::SocialDirectory>,
    metadata_cursor: Mutex<Option<String>>,
    pub voice_db: VoiceDb,
    /// The marketplace readers (ENS names, world-name ownership); `None`
    /// answers empty names and refuses non-admin world moderation.
    pub marketplace: Option<Arc<crate::chain_index::ChainIndex>>,
    /// Shop's source-complete names owner reader for moderation writes.
    pub name_owners: Option<Arc<dyn crate::interfaces::NameOwnerReader>>,
    /// The worlds permission core: world access, protected wallets and
    /// scene ownership; `serve` hands in the ONE instance the worlds routes
    /// share.
    pub policy: Arc<dyn AccessPolicy>,
    /// The ONE room resolver: a world's deployed scenes and the world
    /// mints' rooms.
    pub scenes: Arc<dyn SceneIdentity>,
    /// The live world presence the LiveKit webhook feeds and the world
    /// mints' capacity check reads; `serve` shares it with worlds.
    pub presence: Arc<crate::presence::Presence>,
    /// Contract-wallet (EIP-1654) signature check for signed-fetch chains;
    /// `None` refuses such chains.
    pub eip1654: Option<Arc<dyn Eip1654Validator>>,
    pub caches: admission::scene::SceneCaches,
    /// Where the LiveKit webhook's participant events are also handed.
    pub room_events: Option<Arc<dyn RoomEventSink>>,
    /// Where every verified participant event is stored before it is acknowledged.
    pub room_event_store: Option<Arc<dyn events::RoomEventStore>>,
    webhooks: webhook::WebhookCoordinator,
    /// The Pulse room authority, mounted when `PULSE_ROOM_AUTHORITY_KEY` is set.
    pub relay_authority: Option<Arc<admission::relay_authority::RelayAuthority>>,
    /// The world comms mints' throttle and capacity (`admission::world_mint`).
    pub world_mint: admission::world_mint::MintState,
    pub(crate) world_access_cursor: std::sync::Mutex<Option<String>>,
    /// The island feed's settings (`admission::cluster`); the subscriber starts with
    /// [`Self::start_island_feed`].
    pub cluster: admission::cluster::ClusterConfig,
    /// `serve`'s one NATS link (`NATS_URL`), whether or not the island subscriber runs: the
    /// subscriber and the `/ws` connector both use it, and whichever starts first dials it.
    pub bus: Arc<dyn bus::NatsBus>,
    /// The realm-lane fence, with a v4 audience: `/island-refresh`, the
    /// relay authority's island rooms and the subscriber's fenced path.
    assignments: Option<Arc<admission::cluster::fence::PgAssignmentFence>>,
    /// The island subscriber [`Self::start_island_feed`] started. Weak: its job owns it, and it
    /// holds this state through its gateway.
    island_feed: Mutex<Weak<admission::cluster::subscriber::ClusterSubscriber>>,
    pub(crate) island_safety: admission::cluster::safety::SafetyStore,
}

pub type SharedState = Arc<CommsState>;

impl CommsState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: PgPool,
        http: Outbound,
        settings: Settings,
        marketplace: Option<Arc<crate::chain_index::ChainIndex>>,
        eip1654: Option<Arc<dyn Eip1654Validator>>,
        policy: Arc<dyn AccessPolicy>,
        scenes: Arc<dyn SceneIdentity>,
        presence: Arc<crate::presence::Presence>,
        social: Arc<dyn crate::interfaces::SocialDirectory>,
    ) -> Self {
        Self {
            deny_list: crate::deny_list::DenyList::new(None, http.clone()),
            moderation: Moderation::with_social(pool.clone(), social.clone()),
            bans: Arc::new(Moderation::new(pool.clone())),
            social,
            metadata_cursor: Mutex::new(None),
            voice_db: VoiceDb::new(pool.clone(), settings.voice),
            livekit: settings.livekit.clone(),
            policy,
            scenes,
            presence,
            island_safety: admission::cluster::safety::SafetyStore::new(pool.clone()),
            pool,
            http,
            settings,
            marketplace,
            name_owners: None,
            eip1654,
            caches: admission::scene::SceneCaches::default(),
            room_events: None,
            room_event_store: None,
            webhooks: webhook::WebhookCoordinator::default(),
            relay_authority: None,
            world_mint: admission::world_mint::MintState::default(),
            world_access_cursor: std::sync::Mutex::new(None),
            cluster: admission::cluster::ClusterConfig::default(),
            bus: Arc::new(bus::DisabledBus),
            assignments: None,
            island_feed: Mutex::new(Weak::new()),
        }
    }

    pub fn with_ban_status(mut self, bans: Arc<dyn moderation::BanStatus>) -> Self {
        self.bans = bans;
        self
    }

    pub fn with_name_owner_reader(
        mut self,
        reader: Arc<dyn crate::interfaces::NameOwnerReader>,
    ) -> Self {
        self.name_owners = Some(reader);
        self
    }

    /// The configured external wallet deny list (DENY_LIST_URL).
    pub fn with_deny_list_url(mut self, url: Option<String>) -> Self {
        self.deny_list = crate::deny_list::DenyList::new(url, self.http.clone());
        self
    }

    /// The island feed's settings, the NATS link of `NATS_URL` and, with a
    /// v4 audience, the realm-lane fence. Before
    /// [`Self::with_room_authority_key`], whose island rooms read the fence.
    pub fn with_cluster(
        mut self,
        config: admission::cluster::ClusterConfig,
    ) -> anyhow::Result<Self> {
        let config = config.normalized();
        self.island_safety = admission::cluster::safety::SafetyStore::new(self.pool.clone());
        self.assignments = admission::cluster::fence_for(self.pool.clone(), &config)?;
        self.bus = match config.nats_url.clone() {
            Some(url) => Arc::new(bus::BrokerBus::new(Some(url), "catalyst-comms")),
            None => {
                tracing::warn!(
                    "NATS_URL is unset \u{2014} no island assignment can reach a client: the \
                     island subscriber mints from Pulse's cluster feed, and the /ws connector \
                     computes nothing itself"
                );
                Arc::new(bus::DisabledBus)
            }
        };
        self.cluster = config;
        Ok(self)
    }

    /// Replaces the NATS link: a test's in-process bus.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_bus(mut self, bus: Arc<dyn bus::NatsBus>) -> Self {
        self.bus = bus;
        self
    }

    pub fn assignment_reader(
        &self,
    ) -> Option<Arc<dyn admission::cluster::fence::RealmAssignmentReader>> {
        self.assignments
            .clone()
            .map(|f| f as Arc<dyn admission::cluster::fence::RealmAssignmentReader>)
    }

    pub fn with_room_event_sink(mut self, sink: Arc<dyn RoomEventSink>) -> Self {
        self.room_events = Some(sink);
        self
    }

    pub fn with_room_event_store(mut self, store: Arc<dyn events::RoomEventStore>) -> Self {
        self.room_event_store = Some(store);
        self
    }

    /// Mounts the Pulse room authority with `PULSE_ROOM_AUTHORITY_KEY` (32
    /// base64url bytes, distinct from the LiveKit secret); `None` leaves the
    /// route unmounted and the application relay unusable.
    pub fn with_room_authority_key(mut self, encoded_key: Option<&str>) -> anyhow::Result<Self> {
        let Some(encoded) = encoded_key.map(str::trim).filter(|k| !k.is_empty()) else {
            tracing::info!(
                "PULSE_ROOM_AUTHORITY_KEY unset; the Pulse room authority is not mounted (application relay off)"
            );
            return Ok(self);
        };
        let service_key = admission::relay_authority::parse_service_key(
            encoded,
            self.livekit.api_secret.as_bytes(),
        )?;
        let policy = Arc::new(
            admission::relay_authority::policy::CurrentRoomPolicy::new(
                self.scenes.clone(),
                self.policy.clone(),
                self.bans.clone(),
                self.settings.authoritative_server_address.clone(),
            )
            .with_assignments(self.assignment_reader()),
        );
        let authority = admission::relay_authority::RelayAuthority::new(
            policy,
            self.livekit.api_key.clone(),
            self.livekit.api_secret.as_bytes().to_vec(),
            service_key,
        )
        .map_err(|e| anyhow!("PULSE_ROOM_AUTHORITY_KEY: {e}"))?;
        self.relay_authority = Some(Arc::new(authority));
        tracing::info!(
            endpoint = crate::protocol::room_authority::AUTHORITY_PATH,
            "Pulse room authority mounted"
        );
        Ok(self)
    }

    /// `MAX_USERS_PER_WORLD` for the world comms mints (100 by default).
    pub fn with_max_users_per_world(mut self, max: i64) -> Self {
        self.world_mint.max_users_per_world = max.max(1);
        self
    }

    /// `SHARED_SECRET_MAX_ATTEMPTS_PER_MINUTE` for the world comms mints (3 by default).
    pub fn with_shared_secret_max_attempts(mut self, max: u32) -> Self {
        self.world_mint.max_secret_attempts = max.max(1);
        self
    }

    pub fn eip1654(&self) -> Option<&dyn Eip1654Validator> {
        self.eip1654.as_deref()
    }

    /// The RoomService client over the shared unguarded client (the API host
    /// is operator configuration, commonly a private address).
    /// Read-only local recovery runs in every serving replica, independently
    /// of fleet-wide jobs and the primary connector assignment subscriber.
    pub fn spawn_presence_recovery(self: &Arc<Self>, jobs: &Jobs) {
        self.presence.spawn_recovery(self.clone(), jobs);
    }

    pub fn room_service(&self) -> livekit::RoomServiceClient<'_> {
        livekit::RoomServiceClient::new(
            self.http.raw(),
            &self.livekit.api_url,
            &self.livekit.api_key,
            &self.livekit.api_secret,
        )
    }

    pub fn ingress(&self) -> livekit::IngressClient<'_> {
        livekit::IngressClient::new(
            self.http.raw(),
            &self.livekit.api_url,
            &self.livekit.api_key,
            &self.livekit.api_secret,
        )
    }

    /// The `comms-voice-expiration` job: the bounded private-call and
    /// community-room sweeps once a minute, LiveKit rooms torn down after
    /// the rows.
    pub fn spawn_jobs(self: &Arc<Self>, jobs: &Jobs) {
        // Every serve dials the existing link for read-only island validation.
        // Fleet-disabled replicas still start no destructive subscriber/job.
        self.bus.connect();
        streaming::spawn_jobs(self, jobs);
        removal::spawn_jobs(self, jobs);
        let state = self.clone();
        jobs.spawn_periodic_leased(
            "comms-place-maintenance",
            PeriodicCfg::every(Duration::from_secs(60))
                .with_error_backoff(Duration::from_secs(600)),
            move |_ctx| {
                let state = state.clone();
                async move { moderation::maintenance::run_once(&state).await }
            },
        );
        let pool = self.pool.clone();
        jobs.spawn_periodic_leased(
            "comms-secret-attempts-sweep",
            PeriodicCfg::every(Duration::from_secs(300))
                .with_error_backoff(Duration::from_secs(600)),
            move |_ctx| {
                let pool = pool.clone();
                async move {
                    admission::world_mint::sweep_secret_attempts(
                        &pool,
                        chrono::Utc::now().naive_utc(),
                    )
                    .await
                }
            },
        );
        let state = self.clone();
        jobs.spawn_periodic_leased(
            "comms-voice-expiration",
            PeriodicCfg::every(Duration::from_secs(60))
                .with_error_backoff(Duration::from_secs(600)),
            move |_ctx| {
                let state = state.clone();
                async move { voice::expire_voice_chats(&state).await }
            },
        );
    }

    /// Starts the island subscriber (`comms-cluster-subscriber`) with `sockets`, the `/ws`
    /// connector's, attached: what it mints from Pulse's cluster feed reaches them by call, and
    /// [`Self::announce_session`] reaches it. `serve` calls this once the connector is built, in
    /// the primary only: the one process that holds the sockets. Without `NATS_URL` the
    /// subscriber stays idle and no island is minted.
    pub fn start_island_feed(
        self: &Arc<Self>,
        jobs: &Jobs,
        sockets: Arc<dyn admission::cluster::sockets::IslandSockets>,
    ) {
        let gateway = Arc::new(admission::cluster::gateway::StateGateway::new(self.clone()));
        self.start_island_feed_through(jobs, sockets, gateway);
    }

    /// [`Self::start_island_feed`] minting through `gateway` in place of this state's ban store
    /// and LiveKit API: how a test drives a whole announce-to-island round in process.
    pub(crate) fn start_island_feed_through(
        self: &Arc<Self>,
        jobs: &Jobs,
        sockets: Arc<dyn admission::cluster::sockets::IslandSockets>,
        gateway: Arc<dyn admission::cluster::subscriber::ClusterGateway>,
    ) {
        let runtime = admission::cluster::ClusterRuntime::with_gateway(
            self.bus.clone(),
            gateway,
            self.cluster.clone(),
            self.assignments.clone(),
        );
        runtime.subscriber.attach_sockets(sockets);
        runtime.start(jobs);
        *self.island_feed.lock().unwrap() = Arc::downgrade(&runtime.subscriber);
    }

    /// The connector's announcement that `session` of `wallet` holds a live socket: the running
    /// island subscriber asks Pulse for its current island and mints it. Without one (not
    /// started, idle without `NATS_URL`, or stopped) it does nothing.
    pub fn announce_session(&self, wallet: &str, session: &str) {
        let subscriber = self.island_feed.lock().unwrap().upgrade();
        if let Some(subscriber) = subscriber {
            subscriber.announce_connect(wallet, session);
        }
    }

    /// The existing island subscriber authority, without extending its process lifetime.
    #[cfg(any(test, feature = "test-support"))]
    pub fn island_subscriber(&self) -> Weak<admission::cluster::subscriber::ClusterSubscriber> {
        self.island_feed.lock().unwrap().clone()
    }

    /// A test-owned subscriber can exercise final delivery checks without spawning service jobs.
    #[cfg(any(test, feature = "test-support"))]
    pub fn attach_island_subscriber_for_tests(
        &self,
        subscriber: &Arc<admission::cluster::subscriber::ClusterSubscriber>,
    ) {
        *self.island_feed.lock().unwrap() = Arc::downgrade(subscriber);
    }
}

/// Upstream's `tokenAuthMiddleware` paths: every voice route and the DM
/// privacy patch carry the platform service token.
pub(crate) fn is_bearer_gated_path(path: &str) -> bool {
    path.contains("voice-chat") || path.contains("private-messages-privacy")
}

pub(crate) async fn service_token_layer(
    axum::extract::State(state): axum::extract::State<SharedState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if is_bearer_gated_path(req.uri().path())
        && let Err(why) = auth::require_service_token(
            state.settings.gatekeeper_auth_token.as_deref(),
            req.headers(),
        )
    {
        return auth::service_token_error(why).into_response();
    }
    next.run(req).await
}

/// The gatekeeper routes at their old paths (the old gateway served them
/// under `/comms-gatekeeper`; `serve` nests this router there beside
/// social's MLS routes, behind the social writes switch).
pub fn router(state: SharedState) -> Router {
    room_authority_router(&state)
        .route("/ping", get(admission::handlers::ping))
        .route("/status", get(admission::handlers::status))
        .route(
            "/get-scene-adapter",
            post(admission::handlers::get_scene_adapter),
        )
        .route(
            "/get-server-scene-adapter",
            post(admission::handlers::get_server_scene_adapter),
        )
        .route(
            "/scene-participants",
            get(admission::handlers::list_participants),
        )
        .route(
            "/scene-admin",
            get(moderation::handlers::list_admins)
                .post(moderation::handlers::add_admin)
                .delete(moderation::handlers::remove_admin),
        )
        .route(
            "/scene-bans",
            get(moderation::handlers::list_bans)
                .post(moderation::handlers::ban_user)
                .delete(moderation::handlers::unban_user),
        )
        .route(
            "/scene-bans/addresses",
            get(moderation::handlers::list_ban_addresses),
        )
        .route(
            moderation::handlers::WORLD_BAN_STATUS_PATH,
            get(moderation::handlers::world_ban_check),
        )
        .route(
            "/users/{address}/bans",
            get(moderation::handlers::get_user_ban_status)
                .post(moderation::handlers::post_user_ban)
                .delete(moderation::handlers::delete_user_ban),
        )
        .route(
            "/users/{address}/warnings",
            get(moderation::handlers::get_user_warnings)
                .post(moderation::handlers::post_user_warning),
        )
        .route("/bans", get(moderation::handlers::list_all_bans))
        .route("/livekit-webhook", post(webhook::livekit_webhook))
        .route(
            "/private-messages/token",
            get(voice::private_messages_token),
        )
        .route(
            "/users/{address}/private-messages-privacy",
            patch(voice::patch_private_messages_privacy),
        )
        .route(
            "/private-voice-chat",
            post(voice::create_private_voice_chat),
        )
        .route(
            "/private-voice-chat/{id}",
            delete(voice::end_private_voice_chat),
        )
        .route(
            "/users/{address}/voice-chat-status",
            get(voice::get_voice_chat_status),
        )
        .route(
            "/users/{address}/community-voice-chat-status",
            get(voice::check_user_community_status),
        )
        .route(
            "/community-voice-chat",
            post(voice::community_voice_chat_create_or_join),
        )
        .route(
            "/community-voice-chat/status",
            post(voice::community_voice_chat_bulk_status),
        )
        .route(
            "/community-voice-chat/active",
            get(voice::community_voice_chat_active),
        )
        .route(
            "/community-voice-chat/{id}/status",
            get(voice::community_voice_chat_status),
        )
        .route(
            "/community-voice-chat/{id}",
            delete(voice::community_voice_chat_end),
        )
        .route(
            "/community-voice-chat/{id}/users/{address}/speak-request",
            post(voice::community_request_to_speak).delete(voice::community_reject_speak_request),
        )
        .route(
            "/community-voice-chat/{id}/users/{address}/speaker",
            post(voice::community_promote_speaker).delete(voice::community_demote_speaker),
        )
        .route(
            "/community-voice-chat/{id}/users/{address}/mute",
            patch(voice::community_mute_speaker),
        )
        .route(
            "/community-voice-chat/{id}/users/{address}",
            delete(voice::community_kick_player),
        )
        .route(
            streaming::PATH,
            get(streaming::scene_stream_access)
                .post(streaming::scene_stream_access)
                .put(streaming::scene_stream_access)
                .delete(streaming::scene_stream_access),
        )
        .route("/cast/{*rest}", axum::routing::any(streaming::cast_any))
        .route(
            admission::island_refresh::ISLAND_REFRESH_PATH,
            post(admission::island_refresh::refresh).layer(axum::extract::DefaultBodyLimit::max(0)),
        )
        .layer(axum::extract::DefaultBodyLimit::max(512 * 1024))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            service_token_layer,
        ))
        .layer(axum::middleware::from_fn(nul_guard::nul_guard_middleware))
        .layer(axum::middleware::from_fn(cors::cors_middleware))
        .with_state(state)
}

/// `POST /livekit-webhook` alone, with the gatekeeper router's body cap and
/// guards; `serve` mounts it at the root and under `/worlds-content-server`,
/// where WCS received the same webhook.
pub fn webhook_router(state: SharedState) -> Router {
    Router::new()
        .route("/livekit-webhook", post(webhook::livekit_webhook))
        .layer(axum::extract::DefaultBodyLimit::max(512 * 1024))
        .layer(axum::middleware::from_fn(nul_guard::nul_guard_middleware))
        .layer(axum::middleware::from_fn(cors::cors_middleware))
        .with_state(state)
}

/// The Pulse room authority alone (`POST /internal/pulse/room-authority/v1`,
/// the exact path `PULSE_ROOM_AUTHORITY_URL` names), empty when no service
/// key is configured; `serve` merges it at the root beside the nested
/// copy.
pub fn room_authority_router<S: Clone + Send + Sync + 'static>(state: &CommsState) -> Router<S> {
    match state.relay_authority.as_ref() {
        Some(authority) => admission::relay_authority::router(authority.clone()),
        None => Router::new(),
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    #[test]
    fn bearer_gated_paths_match_upstream_token_auth_middleware() {
        for p in [
            "/community-voice-chat",
            "/community-voice-chat/status",
            "/community-voice-chat/active",
            "/community-voice-chat/abc/users/0x1/mute",
            "/private-voice-chat",
            "/private-voice-chat/id123",
            "/users/0xabc/voice-chat-status",
            "/users/0xabc/community-voice-chat-status",
            "/users/0xabc/private-messages-privacy",
        ] {
            assert!(is_bearer_gated_path(p), "{p}");
        }
        for p in [
            "/private-messages/token",
            "/get-scene-adapter",
            "/scene-participants",
            "/scene-bans",
            "/ping",
        ] {
            assert!(!is_bearer_gated_path(p), "{p}");
        }
    }

    #[test]
    fn real_creds_configure_livekit_and_placeholders_need_the_opt_in() {
        let mut raw = RawSettings::defaults();
        raw.livekit_api_key = "APIabc".into();
        raw.livekit_api_secret = "supersecret".into();
        let s = Settings::resolve(raw.clone()).unwrap();
        assert!(s.livekit.configured);
        assert_eq!(
            (s.livekit.client_url.as_str(), s.livekit.api_url.as_str()),
            ("wss://livekit.local", "https://livekit.local")
        );
        for (k, s) in [
            ("devkey", "devsecret"),
            ("DevKey", "DEVSECRET"),
            ("devkey", "supersecret"),
            ("APIabc", "devsecret"),
            ("", ""),
        ] {
            let mut raw = raw.clone();
            raw.livekit_api_key = k.into();
            raw.livekit_api_secret = s.into();
            assert!(
                Settings::resolve(raw.clone()).is_err(),
                "({k:?}, {s:?}) must refuse to boot without LIVEKIT_ALLOW_DEV_CREDS"
            );
            raw.livekit_allow_dev_creds = true;
            let resolved = Settings::resolve(raw).unwrap();
            assert!(!resolved.livekit.configured);
            assert_eq!(
                (
                    resolved.livekit.api_key.as_str(),
                    resolved.livekit.api_secret.as_str()
                ),
                ("devkey", "devsecret")
            );
        }
    }

    /// The refusal names a value the flag's clap `bool` parser takes
    /// (`[possible values: true, false]`): `=1` exited the next boot with
    /// "invalid value '1'".
    #[test]
    fn the_missing_credentials_refusal_suggests_a_value_the_flag_parses() {
        let mut raw = RawSettings::defaults();
        raw.livekit_api_key = "".into();
        raw.livekit_api_secret = "".into();
        let message = Settings::resolve(raw).unwrap_err().to_string();
        let suggested = message
            .split("LIVEKIT_ALLOW_DEV_CREDS=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .expect("the refusal names the opt-in with a value");
        assert!(suggested.parse::<bool>().is_ok(), "{message}");
        assert!(!message.contains("LIVEKIT_ALLOW_DEV_CREDS=1"), "{message}");
    }

    #[test]
    fn endpoints_moderators_and_upstreams_resolve() {
        let mut raw = RawSettings::defaults();
        raw.livekit_allow_dev_creds = true;
        raw.livekit_api_host = Some("http://livekit-api.internal:7880".into());
        raw.livekit_ws_url = Some("wss://livekit.example.com".into());
        raw.moderator_addresses = "0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA, not-an-address\n0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into();
        raw.authoritative_server_address =
            Some("0xCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC".into());
        raw.private_messages_room_id = String::new();
        let s = Settings::resolve(raw.clone()).unwrap();
        assert_eq!(s.livekit.client_url, "wss://livekit.example.com");
        assert_eq!(s.livekit.api_url, "http://livekit-api.internal:7880");
        assert_eq!(s.moderator_addresses.len(), 2);
        assert_eq!(
            s.moderator_addresses[0].as_str(),
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(
            s.authoritative_server_address.as_ref().unwrap().as_str(),
            "0xcccccccccccccccccccccccccccccccccccccccc"
        );
        assert_eq!(s.private_messages_room_id, "private-messages");
        assert_eq!(s.voice.expired_batch_size, 50);
        raw.authoritative_server_address = Some("nope".into());
        assert!(Settings::resolve(raw.clone()).is_err());
        raw.authoritative_server_address = None;
        raw.land_operators_url = Some("ftp://x".into());
        assert!(Settings::resolve(raw).is_err());
    }
}

#[cfg(test)]
mod redaction_tests {
    use super::*;

    #[test]
    fn debug_output_never_carries_the_livekit_secret_or_the_tokens() {
        let mut s = Settings::for_tests();
        s.livekit.api_secret = "livekit-secret-0xabab".into();
        s.moderator_token = Some("mod-0xabab".into());
        s.gatekeeper_auth_token = Some("gk-0xabab".into());
        let shown = format!("{s:?}");
        assert!(!shown.contains("abab"), "{shown}");
        assert!(shown.contains("api_secret: \"<set>\""), "{shown}");
    }
}
