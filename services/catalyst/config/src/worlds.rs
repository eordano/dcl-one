//! The worlds plane's settings: the content mirror, hosting and federation.

use clap::Args;

/// Worlds: the worlds content server mirror and what every world loads.
#[derive(Args, Clone, Debug)]
pub struct WorldsContentConfig {
    // worlds: the worlds content server mirror (scenes and files). The
    // `WORLDS_CONTENT_MIRROR_*` family names the worlds-content-server
    // upstream; `WORLDS_MIRROR_UPSTREAM` above is the places API world list.
    #[arg(long, env = "WORLDS_CONTENT_MIRROR_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub worlds_content_mirror_enabled: bool,

    /// Explicit upstream worlds content server: https:// to a public host,
    /// never this node (the mirror posture's); empty refuses the mirror. With
    /// READ_THROUGH_ENABLED it is also where a world not held, and its files,
    /// are read through from, mirror pass or not.
    #[arg(long, env = "WORLDS_CONTENT_MIRROR_SOURCE", default_value = "")]
    pub worlds_content_mirror_source: String,

    /// Pause between passes; the v1 stack's timer ran every six hours.
    #[arg(
        long,
        env = "WORLDS_CONTENT_MIRROR_INTERVAL_SECS",
        default_value_t = 21600
    )]
    pub worlds_content_mirror_interval_secs: u64,

    /// Worlds mirrored at once (one batch = the cursor granularity).
    #[arg(long, env = "WORLDS_CONTENT_MIRROR_CONCURRENCY", default_value_t = 4)]
    pub worlds_content_mirror_concurrency: usize,

    /// Files fetched at once per scene.
    #[arg(
        long,
        env = "WORLDS_CONTENT_MIRROR_BLOB_CONCURRENCY",
        default_value_t = 8
    )]
    pub worlds_content_mirror_blob_concurrency: usize,

    /// Bounded run: only the first N catalogue names in name order; 0 = all.
    #[arg(long, env = "WORLDS_CONTENT_MIRROR_LIMIT", default_value_t = 0)]
    pub worlds_content_mirror_limit: usize,

    /// Comma-separated world names to mirror; empty = the whole catalogue.
    #[arg(long, env = "WORLDS_CONTENT_MIRROR_NAMES", default_value = "")]
    pub worlds_content_mirror_names: String,

    /// A world read through from WORLDS_CONTENT_MIRROR_SOURCE whose scene was
    /// signed by someone other than the NAME holder is accepted when the
    /// upstream lists the signer on the world's deployment allow-list and names
    /// the owner this node's NAME authority names. The grant is the upstream's
    /// word (its signed grant is not published). Off: only the holder's scenes.
    #[arg(long, env = "WORLDS_READ_THROUGH_TRUST_UPSTREAM_GRANTS", default_value_t = false, action = clap::ArgAction::Set)]
    pub worlds_read_through_trust_upstream_grants: bool,

    /// Strip `.dcl.eth` from the realm name of worlds published here.
    #[arg(long, env = "WORLDS_REALM_NAME_STRIP_ENS", default_value_t = true, action = clap::ArgAction::Set)]
    pub worlds_realm_name_strip_ens: bool,

    /// Whitespace-separated scene URNs every world loads.
    #[arg(long, env = "GLOBAL_SCENES_URN")]
    pub global_scenes_urn: Option<String>,

    /// The estate minimap a world's /about falls back to; empty is this
    /// node's own `<PUBLIC_URL>/api/v1/estatemap.png`.
    #[arg(long, env = "MAP_ESTATE_VIEW_URL", default_value = "")]
    pub map_estate_view_url: String,

    /// Smart-wearable URNs `/world/{name}/preview-wearables` answers for a
    /// world that selected none (comma, space or newline separated).
    #[arg(long, env = "PREVIEW_WEARABLE_URNS")]
    pub preview_wearable_urns: Option<String>,
}

/// Worlds: the write surface, world and player storage, the scene host and archipelago.
#[derive(Args, Clone, Debug)]
pub struct WorldsConfig {
    // worlds: the stage-5 worlds bundle (the old catalyrst-worlds write
    // surface, catalyrst-world-storage, catalyrst-scene-state, archipelago's
    // ws-connector and the worlds federation mirror). A world publish, a
    // permission or settings edit and a storage value are local authority
    // peer sync does not back up, so every worlds write answers 503 until a
    // backup restore has been demonstrated (DATABASE.md, "Bootstrap, schema versions and
    // local writes"); the reads, the admission mints and the sockets serve.
    #[arg(long, env = "WORLDS_WRITES_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub worlds_writes_enabled: bool,

    /// A worlds content server base URL every local world publish is sent on
    /// to, unchanged, at `<url>/entities` once it committed here (the content
    /// lane's engine and FORWARD_* knobs). Unset forwards nothing. A public
    /// http(s) origin with no path that is not this node, checked at startup.
    #[arg(long, env = "WORLDS_FORWARD_UPSTREAM_URL")]
    pub worlds_forward_upstream_url: Option<String>,

    /// Bearer of the worlds `/admin/*` routes and the federation refresh/veto
    /// routes (or CATALYRST_WORLDS_ADMIN_TOKEN_FILE); unset refuses them.
    #[arg(long, env = "CATALYRST_WORLDS_ADMIN_TOKEN", hide_env_values = true)]
    pub catalyrst_worlds_admin_token: Option<String>,

    /// Personal test worlds (`<address>.dcl.eth`) this realm hosts at once; 0 disables them.
    #[arg(long, env = "WORLDS_PERSONAL_WORLDS_MAX", default_value_t = 0)]
    pub worlds_personal_worlds_max: u64,

    /// Size cap of a personal world, bytes (1..=300 MiB).
    #[arg(
        long,
        env = "WORLDS_PERSONAL_WORLD_MAX_SIZE_BYTES",
        default_value_t = 52_428_800
    )]
    pub worlds_personal_world_max_size_bytes: u64,

    /// Parcels a world scene may span unless `worlds.world_limit_overrides` says otherwise.
    #[arg(long, env = "MAX_PARCELS", default_value_t = 4, value_parser = clap::value_parser!(u32).range(1..))]
    pub max_parcels: u32,

    /// Publish and own ENS (`*.eth`) worlds, their holder read on chain over ETH_RPC_URL.
    #[arg(long, env = "ALLOW_ENS_DOMAINS", default_value_t = false, action = clap::ArgAction::Set)]
    pub allow_ens_domains: bool,

    /// Size cap of an ENS world, MiB, unless `worlds.world_limit_overrides` says otherwise.
    #[arg(long, env = "ENS_MAX_SIZE", default_value_t = 36, value_parser = clap::value_parser!(u32).range(1..=300))]
    pub ens_max_size: u32,

    /// Pause between passes of the job that rewrites `worlds.owner` from the name authority.
    #[arg(
        long,
        env = "WORLDS_OWNER_RECONCILE_INTERVAL_SECS",
        default_value_t = 12 * 3600,
        value_parser = clap::value_parser!(u64).range(60..)
    )]
    pub worlds_owner_reconcile_interval_secs: u64,

    /// Wallets a world's access allow-list may hold (1..=10,000; 0 would refuse every addition).
    #[arg(
        long,
        env = "ACCESS_MAX_WALLETS",
        default_value_t = 1000,
        value_parser = clap::value_parser!(u32).range(1..=10_000)
    )]
    pub access_max_wallets: u32,

    /// Communities a world's access allow-list may hold (1..=1,000; 0 would refuse every addition).
    #[arg(
        long,
        env = "ACCESS_MAX_COMMUNITIES",
        default_value_t = 50,
        value_parser = clap::value_parser!(u32).range(1..=1_000)
    )]
    pub access_max_communities: u32,

    /// Entries a deployed entity's `content` may list, at most the 10,000 file parts a deploy may upload.
    #[arg(long, env = "MAX_FILE_COUNT", default_value_t = 10_000, value_parser = clap::value_parser!(u32).range(1..=10_000))]
    pub max_file_count: u32,

    /// Deploy payload bytes buffered at once across every upload (>= one 350 MiB deploy).
    #[arg(
        long,
        env = "MAX_IN_FLIGHT_UPLOAD_BYTES",
        default_value_t = 4_294_967_296
    )]
    pub max_in_flight_upload_bytes: u64,

    #[arg(long, env = "MAX_CONCURRENT_UPLOADS", default_value_t = 40)]
    pub max_concurrent_uploads: u64,

    #[arg(long, env = "MAX_IN_FLIGHT_UPLOAD_FILES", default_value_t = 40_000)]
    pub max_in_flight_upload_files: u64,

    /// How long a replaced or undeployed world scene stays resolvable for
    /// scene comms and its content stays out of the blob collector (7 days).
    #[arg(long, env = "SCENE_EVICTION_TTL_MS", default_value_t = 604_800_000)]
    pub scene_eviction_ttl_ms: u64,

    #[arg(long, env = "MULTIPART_UPLOAD_TIMEOUT_MS", default_value_t = 300_000)]
    pub multipart_upload_timeout_ms: u64,

    #[arg(
        long,
        env = "DEPLOYMENT_PROCESSING_TIMEOUT_MS",
        default_value_t = 300_000
    )]
    pub deployment_processing_timeout_ms: u64,

    // PENDING_DEPLOYMENT_TTL, MAX_PENDING_DEPLOYMENTS_PER_DEPLOYER, MAX_PENDING_BYTES_PER_DEPLOYER,
    // MAX_PENDING_BYTES and MAX_PARTIAL_UPLOAD_BYTES_PER_MINUTE are read from `content`.
    /// How often expired partial uploads are cleaned up, ms.
    #[arg(
        long,
        env = "PARTIAL_UPLOAD_CLEANUP_INTERVAL_MS",
        default_value_t = 300_000
    )]
    pub partial_upload_cleanup_interval_ms: u64,

    /// Live partial uploads per world; one deployer may hold all but one of them.
    #[arg(long, env = "MAX_PENDING_DEPLOYMENTS_PER_WORLD", default_value_t = 4)]
    pub max_pending_deployments_per_world: u64,

    /// Staged content bytes per world, as a multiple of the world's scene size cap.
    #[arg(long, env = "PENDING_BYTES_PER_WORLD_FACTOR", default_value_t = 2)]
    pub pending_bytes_per_world_factor: u64,

    /// Staged bytes the live uploads started from one client source may hold
    /// (>= MAX_PENDING_BYTES_PER_DEPLOYER).
    #[arg(
        long,
        env = "MAX_PENDING_BYTES_PER_SOURCE",
        default_value_t = 4_294_967_296
    )]
    pub max_pending_bytes_per_source: u64,

    /// How long a completed partial upload answers its retried final batch, ms.
    #[arg(long, env = "COMPLETED_UPLOAD_TTL", default_value_t = 86_400_000)]
    pub completed_upload_ttl: u64,

    /// The Builder whose `/worlds?tab=dcl` page world notifications link to.
    #[arg(
        long,
        env = "BUILDER_URL",
        default_value = "https://decentraland.org/builder"
    )]
    pub builder_url: String,

    /// 64 hex characters (or ENCRYPTION_KEY_FILE): the AES-256-GCM key of the
    /// env values. Required: `serve` refuses to start without it.
    #[arg(long, env = "ENCRYPTION_KEY", hide_env_values = true)]
    pub encryption_key: Option<String>,

    /// Comma-separated wallets authorized on every storage route.
    #[arg(long, env = "AUTHORIZED_ADDRESSES", default_value = "")]
    pub authorized_addresses: String,

    /// The per-process LRU of single storage values: on unless exactly
    /// `false`. Another process's write is seen after at most
    /// STORAGE_CACHE_TTL_SECONDS (no cross-process invalidation).
    #[arg(long, env = "STORAGE_CACHE_ENABLED")]
    pub storage_cache_enabled: Option<String>,

    #[arg(long, env = "STORAGE_CACHE_TTL_SECONDS", default_value_t = 60)]
    pub storage_cache_ttl_seconds: u64,

    #[arg(long, env = "STORAGE_CACHE_MAX", default_value_t = 8_000)]
    pub storage_cache_max: u64,

    #[arg(long, env = "STORAGE_CACHE_MAX_VALUE_BYTES", default_value_t = 32_768)]
    pub storage_cache_max_value_bytes: u64,

    #[arg(
        long,
        env = "ENV_STORAGE_MAX_VALUE_SIZE_BYTES",
        default_value_t = 10_240
    )]
    pub env_storage_max_value_size_bytes: i64,

    #[arg(
        long,
        env = "ENV_STORAGE_MAX_TOTAL_SIZE_BYTES",
        default_value_t = 262_144
    )]
    pub env_storage_max_total_size_bytes: i64,

    #[arg(
        long,
        env = "WORLD_STORAGE_MAX_VALUE_SIZE_BYTES",
        default_value_t = 524_288
    )]
    pub world_storage_max_value_size_bytes: i64,

    #[arg(
        long,
        env = "WORLD_STORAGE_MAX_TOTAL_SIZE_BYTES",
        default_value_t = 10_485_760
    )]
    pub world_storage_max_total_size_bytes: i64,

    #[arg(
        long,
        env = "PLAYER_STORAGE_MAX_VALUE_SIZE_BYTES",
        default_value_t = 102_400
    )]
    pub player_storage_max_value_size_bytes: i64,

    #[arg(
        long,
        env = "PLAYER_STORAGE_MAX_TOTAL_SIZE_BYTES",
        default_value_t = 1_048_576
    )]
    pub player_storage_max_total_size_bytes: i64,

    /// `POST /world-storage/exports` running at once per scene, per process.
    #[arg(long, env = "EXPORT_MAX_CONCURRENT_PER_SCENE", default_value_t = 1)]
    pub export_max_concurrent_per_scene: u64,

    /// Exports running at once in this process (each holds a pool connection).
    #[arg(long, env = "EXPORT_MAX_CONCURRENT_PER_POD", default_value_t = 5)]
    pub export_max_concurrent_per_pod: u64,

    /// Seconds after an export starts before the same scene may start another.
    #[arg(long, env = "EXPORT_SCENE_COOLDOWN_SECONDS", default_value_t = 10)]
    pub export_scene_cooldown_seconds: u64,

    /// Seconds from an export's admission before its stream is aborted.
    #[arg(long, env = "EXPORT_TIMEOUT_SECONDS", default_value_t = 120)]
    pub export_timeout_seconds: u64,

    /// Cap on an export's raw (uncompressed) bytes.
    #[arg(long, env = "EXPORT_MAX_SIZE_BYTES", default_value_t = 524_288_000)]
    pub export_max_size_bytes: u64,

    /// Milliseconds each export statement may run (`statement_timeout`; 0 disables it).
    #[arg(
        long,
        env = "PG_COMPONENT_STREAM_QUERY_TIMEOUT",
        default_value_t = 120_000
    )]
    pub pg_component_stream_query_timeout: u64,

    // worlds/scene_state: the authoritative SDK7 scene host (the old
    // catalyrst-scene-state; V8 isolates in this process).
    /// Construct and mount the scene host under /scene-state.
    #[arg(long, env = "SCENE_HOST_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub scene_host_enabled: bool,

    /// A local `game.js` served as `localScene` (development).
    #[arg(long, env = "SCENE_HOST_LOCAL_SCENE_PATH")]
    pub scene_host_local_scene_path: Option<String>,

    /// Secret of POST /scene-state/debugging/reload (or _FILE); unset refuses it.
    #[arg(long, env = "SCENE_HOST_DEBUGGING_SECRET", hide_env_values = true)]
    pub scene_host_debugging_secret: Option<String>,

    /// Bearer of /scene-state/admin/scene/* (or _FILE); falls back to the debugging secret.
    #[arg(long, env = "SCENE_HOST_ADMIN_TOKEN", hide_env_values = true)]
    pub scene_host_admin_token: Option<String>,

    /// Seconds a socket gets to send its Auth frame.
    #[arg(long, env = "SCENE_HOST_AUTH_TIMEOUT_SECS", default_value_t = 5)]
    pub scene_host_auth_timeout_secs: u64,

    /// Run every scene as a plain CRDT relay (no V8).
    #[arg(long, env = "SCENE_HOST_DISABLE_JS_RUNTIME", default_value_t = false, action = clap::ArgAction::Set)]
    pub scene_host_disable_js_runtime: bool,

    #[arg(long, env = "JS_HEAP_LIMIT_MB", default_value_t = 384)]
    pub js_heap_limit_mb: usize,

    #[arg(long, env = "JS_TICK_BUDGET_MS", default_value_t = 250)]
    pub js_tick_budget_ms: u64,

    #[arg(long, env = "JS_SHUTDOWN_JOIN_MS", default_value_t = 2000)]
    pub js_shutdown_join_ms: u64,

    #[arg(long, env = "JS_UPDATE_FAILURE_CAP", default_value_t = 30)]
    pub js_update_failure_cap: usize,

    #[arg(long, env = "CLIENT_OUTBOUND_MAX", default_value_t = 1024)]
    pub client_outbound_max: usize,

    #[arg(long, env = "CLIENT_INBOUND_MAX", default_value_t = 1024)]
    pub client_inbound_max: usize,

    #[arg(long, env = "CRDT_MAX_COMPONENTS", default_value_t = 100_000)]
    pub crdt_max_components: usize,

    #[arg(long, env = "WS_MAX_FRAME_BYTES", default_value_t = 2_097_152)]
    pub ws_max_frame_bytes: usize,

    /// The scene host's largest single component payload a scene's state keeps.
    #[arg(long, env = "CRDT_MAX_VALUE_BYTES", default_value_t = 262_144)]
    pub crdt_max_value_bytes: usize,

    /// The scene host's payload bytes one scene's state keeps.
    #[arg(long, env = "CRDT_MAX_BYTES", default_value_t = 33_554_432)]
    pub crdt_max_bytes: usize,

    /// The scene host's unread client bytes queued toward the scene, per client.
    #[arg(long, env = "CLIENT_INBOUND_MAX_BYTES", default_value_t = 8_388_608)]
    pub client_inbound_max_bytes: usize,

    /// The scene host's unread client bytes queued toward one scene.
    #[arg(long, env = "SCENE_INBOUND_MAX_BYTES", default_value_t = 33_554_432)]
    pub scene_inbound_max_bytes: usize,

    /// The scene host's scene bytes queued to one client.
    #[arg(long, env = "CLIENT_OUTBOUND_MAX_BYTES", default_value_t = 8_388_608)]
    pub client_outbound_max_bytes: usize,

    /// The scene host's ArrayBuffer memory one isolate holds outside its JS heap.
    #[arg(long, env = "JS_ARRAY_BUFFER_LIMIT_MB", default_value_t = 128)]
    pub js_array_buffer_limit_mb: usize,

    /// The world-storage origin, the ONLY origin a scene's `~system/SignedFetch`
    /// may reach (this node's `<PUBLIC_URL>/world-storage`); unset disables scene storage.
    #[arg(long, env = "SCENE_HOST_STORAGE_URL")]
    pub scene_host_storage_url: Option<String>,

    /// Admit an `http://` loopback storage origin (development).
    #[arg(long, env = "SCENE_HOST_STORAGE_ALLOW_HTTP", default_value_t = false, action = clap::ArgAction::Set)]
    pub scene_host_storage_allow_http: bool,

    /// The authoritative server's private key (or _FILE): storage
    /// delegations are minted in-process; its address is AUTHORITATIVE_SERVER_ADDRESS.
    #[arg(long, env = "SCENE_HOST_AUTHORITATIVE_KEY", hide_env_values = true)]
    pub scene_host_authoritative_key: Option<String>,

    #[arg(long, env = "SCENE_HOST_DELEGATION_TTL_SECS", default_value_t = 3600)]
    pub scene_host_delegation_ttl_secs: u64,

    /// The external delegation minter when the key stays outside this process.
    #[arg(long, env = "SCENE_HOST_DELEGATION_MINTER_URL")]
    pub scene_host_delegation_minter_url: Option<String>,

    #[arg(
        long,
        env = "SCENE_HOST_DELEGATION_MINTER_TOKEN",
        hide_env_values = true
    )]
    pub scene_host_delegation_minter_token: Option<String>,

    /// A pre-minted base64 v1 delegation envelope (development; never renewed).
    #[arg(long, env = "SCENE_HOST_STORAGE_DELEGATION", hide_env_values = true)]
    pub scene_host_storage_delegation: Option<String>,

    #[arg(
        long,
        env = "SIGNED_FETCH_MAX_RESPONSE_BYTES",
        default_value_t = 2_097_152
    )]
    pub signed_fetch_max_response_bytes: usize,

    #[arg(long, env = "SIGNED_FETCH_MAX_BODY_BYTES", default_value_t = 1_048_576)]
    pub signed_fetch_max_body_bytes: usize,

    #[arg(long, env = "SIGNED_FETCH_MAX_IN_FLIGHT", default_value_t = 8)]
    pub signed_fetch_max_in_flight: usize,

    #[arg(long, env = "SIGNED_FETCH_TIMEOUT_MS", default_value_t = 10_000)]
    pub signed_fetch_timeout_ms: u64,

    /// Hosted scenes at once (one V8 isolate and up to JS_HEAP_LIMIT_MB each).
    #[arg(long, env = "SCENE_HOST_MAX_SCENES", default_value_t = 64)]
    pub scene_host_max_scenes: usize,

    /// Load, for an admitted client, an unhosted scene with a local active
    /// deployment that sets `authoritativeMultiplayer`.
    #[arg(long, env = "SCENE_HOST_AUTOLOAD", default_value_t = true, action = clap::ArgAction::Set)]
    pub scene_host_autoload: bool,

    /// The re-admission and redeploy sweep interval; 0 disables it.
    #[arg(long, env = "SCENE_HOST_SWEEP_INTERVAL_SECS", default_value_t = 30)]
    pub scene_host_sweep_interval_secs: u64,

    /// Unload a hosted scene with no clients for this long; 0 disables it.
    #[arg(long, env = "SCENE_HOST_IDLE_UNLOAD_SECS", default_value_t = 300)]
    pub scene_host_idle_unload_secs: u64,

    // worlds/archipelago: the ws-connector (the old catalyrst-archipelago
    // names); LiveKit comes from the comms settings, the pool is the ONE pool.
    /// Process-wide connector socket cap, including pending handshakes.
    #[arg(long, env = "WS_MAX_SOCKETS", default_value_t = 3000, value_parser = clap::value_parser!(u32).range(1..))]
    pub ws_max_sockets: u32,

    /// Connector sockets one client address may hold, pending handshakes included; 0 is unlimited.
    #[arg(long, env = "WS_MAX_SOCKETS_PER_IP", default_value_t = 64)]
    pub ws_max_sockets_per_ip: u32,

    /// Handshake deadline, ms; 0 keeps the default.
    #[arg(long, env = "HANDSHAKE_TIMEOUT", default_value_t = 60_000)]
    pub handshake_timeout: u64,

    /// The wallet denylist document; unset disarms the external denylist.
    /// `NODE_POSTURE=mirror` defaults it to Decentraland's, as upstream's
    /// `DENYLIST_JSON_URL` defaults.
    #[arg(long, env = "DENY_LIST_URL")]
    pub deny_list_url: Option<String>,

    /// Upstream worlds-content-server's name for `DENY_LIST_URL`, read when
    /// that one is unset or empty.
    #[arg(long, env = "DENYLIST_JSON_URL", hide = true)]
    pub denylist_json_url: Option<String>,

    /// Comma-separated full world names refused as the DAO name deny list
    /// refuses a name (upstream's `BLOCKED_WORLD_NAMES`): each entry trimmed
    /// and lowercased, compared with the whole lowercased name, and applied
    /// even while that list cannot be read.
    #[arg(long, env = "BLOCKED_WORLD_NAMES", default_value = "")]
    pub blocked_world_names: String,

    /// The NATS broker `serve` (comms' one link) and `catalyst pulse` meet on; unset, no island is ever delivered.
    #[arg(long, env = "NATS_URL")]
    pub nats_url: Option<String>,

    #[arg(
        long,
        env = "ISLAND_CHANGED_DEDUP_MS",
        default_value = "10000",
        value_parser = parse_island_changed_dedup_ms,
        allow_hyphen_values = true
    )]
    pub island_changed_dedup_ms: f64,

    #[arg(long, env = "CLUSTER_TAKEOVER_RETRY_DELAY_MS", default_value_t = 100)]
    pub cluster_takeover_retry_delay_ms: u64,

    #[arg(long, env = "CLUSTER_DRAIN_TIMEOUT_MS", default_value_t = 5_000)]
    pub cluster_drain_timeout_ms: u64,

    /// Island JWT lifetime, capped at 60 s (the takeover quarantine waits it out).
    #[arg(long, env = "CLUSTER_ISLAND_TOKEN_TTL_SECONDS", default_value_t = 60)]
    pub cluster_island_token_ttl_seconds: u64,

    /// Wait out old island JWTs when a same-room owner is replaced; disable
    /// only with LiveKit Cloud's cutoff-based token revocation.
    #[arg(long, env = "CLUSTER_SELF_HOSTED_TOKEN_QUARANTINE", default_value_t = true, action = clap::ArgAction::Set)]
    pub cluster_self_hosted_token_quarantine: bool,

    #[arg(long, env = "CLUSTER_PEER_STATE_MAX", default_value_t = 20_000)]
    pub cluster_peer_state_max: usize,

    #[arg(long, env = "CLUSTER_PEER_STATE_TTL_MS", default_value_t = 3_600_000)]
    pub cluster_peer_state_ttl_ms: u64,

    /// The world comms mints refuse a world holding this many participants.
    #[arg(long, env = "MAX_USERS_PER_WORLD", default_value_t = 100)]
    pub max_users_per_world: i64,

    /// Wrong shared secrets a caller may present per world and minute before the mints answer 429.
    #[arg(
        long,
        env = "SHARED_SECRET_MAX_ATTEMPTS_PER_MINUTE",
        default_value_t = 3
    )]
    pub shared_secret_max_attempts_per_minute: u32,

    /// The deployment audience clients sign for /ws/v4; unset leaves the v4 control authority unavailable.
    #[arg(long, env = "ARCHIPELAGO_CONTROL_V4_AUDIENCE")]
    pub archipelago_control_v4_audience: Option<String>,
}

impl WorldsConfig {
    /// The wallet denylist document: `DENY_LIST_URL`, else `DENYLIST_JSON_URL`.
    pub fn deny_list(&self) -> Option<String> {
        crate::config::Config::non_empty(&self.deny_list_url)
            .or_else(|| crate::config::Config::non_empty(&self.denylist_json_url))
    }
}

/// The worlds federation: the peer file and its caps.
#[derive(Args, Clone, Debug)]
pub struct WorldsFedConfig {
    // worlds/fed: the worlds federation peer file and its caps.
    /// The federation-peers.toml; unset turns federation off (routes 503, nothing polled).
    #[arg(long, env = "WORLDS_FED_PEERS_FILE")]
    pub worlds_fed_peers_file: Option<std::path::PathBuf>,

    #[arg(long, env = "WORLDS_FED_POLL_INTERVAL_SECS", default_value_t = 300)]
    pub worlds_fed_poll_interval_secs: u64,

    #[arg(long, env = "WORLDS_FED_MAX_RESPONSE_BYTES", default_value_t = 4 * 1024 * 1024)]
    pub worlds_fed_max_response_bytes: u64,

    #[arg(long, env = "WORLDS_FED_MAX_WORLDS_PER_PEER", default_value_t = 10_000)]
    pub worlds_fed_max_worlds_per_peer: u64,

    #[arg(long, env = "WORLDS_FED_ALLOW_INSECURE_LOOPBACK_PEERS", default_value_t = false, action = clap::ArgAction::Set)]
    pub worlds_fed_allow_insecure_loopback_peers: bool,
}

/// Decimal prefix accepted by the connector's numeric environment setting, including
/// signed infinity. Only ECMAScript whitespace is skipped; an absent value is defaulted
/// by the caller, while an explicitly empty value is invalid.
pub fn parse_island_changed_dedup_ms(input: &str) -> Result<f64, String> {
    let value = input.trim_start_matches(|c| {
        matches!(c,
        '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' |
        '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' |
        '\u{205f}' | '\u{3000}' | '\u{feff}')
    });
    let bytes = value.as_bytes();
    let mut end = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    if value[end..].starts_with("Infinity") {
        return Ok(if bytes.first() == Some(&b'-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    let mut digits = 0;
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
        digits += 1;
    }
    if bytes.get(end) == Some(&b'.') {
        end += 1;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return Err("ISLAND_CHANGED_DEDUP_MS must start with a number".into());
    }
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let mut exponent = end + 1;
        if matches!(bytes.get(exponent), Some(b'+' | b'-')) {
            exponent += 1;
        }
        let start = exponent;
        while bytes.get(exponent).is_some_and(u8::is_ascii_digit) {
            exponent += 1;
        }
        if exponent > start {
            end = exponent;
        }
    }
    value[..end]
        .parse::<f64>()
        .map_err(|_| "ISLAND_CHANGED_DEDUP_MS must start with a number".into())
}

#[cfg(test)]
mod tests {
    use crate::config::Config;

    fn parse(args: &[&str]) -> Result<Config, clap::Error> {
        Config::parse_for_tests(args)
    }
    #[test]
    fn island_dedup_uses_ecmascript_numeric_prefixes() {
        for (text, expected) in [
            (".5", 0.5_f64),
            ("1.", 1.0),
            ("+12.5ms", 12.5),
            ("1e2suffix", 100.0),
            ("1e", 1.0),
            ("1e+", 1.0),
            ("0x10", 0.0),
            ("-0", -0.0),
            ("-1e-9999", -0.0),
            ("1e9999", f64::INFINITY),
            ("+Infinitysuffix", f64::INFINITY),
            ("-Infinity", f64::NEG_INFINITY),
            ("\u{feff}\u{2028}2", 2.0),
        ] {
            let config = parse(&[
                "catalyst",
                "--database-url",
                "postgres://localhost/test",
                "--island-changed-dedup-ms",
                text,
            ])
            .unwrap_or_else(|e| panic!("{text:?}: {e}"));
            assert_eq!(
                config.worlds.island_changed_dedup_ms.to_bits(),
                expected.to_bits(),
                "{text:?}"
            );
        }
        for text in [
            "",
            " ",
            "NaN",
            "inf",
            "infinity",
            "+",
            ".",
            "\u{85}1",
            "\u{180e}1",
        ] {
            assert!(
                parse(&[
                    "catalyst",
                    "--database-url",
                    "postgres://localhost/test",
                    "--island-changed-dedup-ms",
                    text
                ])
                .is_err(),
                "{text:?}"
            );
        }
    }
    #[test]
    fn negative_island_dedup_configuration_is_accepted() {
        let config = parse(&[
            "catalyst",
            "--database-url",
            "postgres://localhost/test",
            "--island-changed-dedup-ms",
            "-1",
        ])
        .unwrap();
        assert_eq!(config.worlds.island_changed_dedup_ms, -1.0);
    }

    #[test]
    fn storage_cache_flag_keeps_any_string_for_the_exact_false_check() {
        // Upstream reads it as a string; only `false` disables (Settings::resolve).
        for value in ["true", "false", "off", "0"] {
            let config = parse(&[
                "catalyst",
                "--database-url",
                "postgres://localhost/test",
                "--storage-cache-enabled",
                value,
            ])
            .unwrap();
            assert_eq!(config.worlds.storage_cache_enabled.as_deref(), Some(value));
        }
    }

    #[test]
    fn export_settings_accept_zero_like_upstream() {
        let config = parse(&[
            "x",
            "--database-url",
            "postgres:///c",
            "--export-timeout-seconds",
            "0",
            "--export-max-concurrent-per-pod",
            "0",
            "--pg-component-stream-query-timeout",
            "0",
        ])
        .unwrap();
        assert_eq!(
            (
                config.worlds.export_timeout_seconds,
                config.worlds.export_max_concurrent_per_pod,
                config.worlds.pg_component_stream_query_timeout
            ),
            (0, 0, 0)
        );
    }

    #[test]
    fn blocked_world_names_defaults_empty_and_keeps_the_raw_list() {
        let defaults = parse(&["x", "--database-url", "postgres:///c"]).unwrap();
        assert_eq!(defaults.worlds.blocked_world_names, "");
        let config = parse(&[
            "x",
            "--database-url",
            "postgres:///c",
            "--blocked-world-names",
            " Blocked.dcl.eth, , other.eth",
        ])
        .unwrap();
        assert_eq!(
            config.worlds.blocked_world_names,
            " Blocked.dcl.eth, , other.eth"
        );
    }

    #[test]
    fn max_file_count_is_capped_at_the_deploy_file_parts() {
        let with = |v: &str| {
            parse(&[
                "x",
                "--database-url",
                "postgres:///c",
                "--max-file-count",
                v,
            ])
        };
        assert_eq!(with("10000").unwrap().worlds.max_file_count, 10_000);
        assert!(with("10001").is_err());
        assert!(with("0").is_err());
    }

    #[test]
    fn access_allow_list_caps_keep_the_upstream_defaults_within_a_range() {
        let with = |flag: &str, v: &str| parse(&["x", "--database-url", "postgres:///c", flag, v]);
        let defaults = parse(&["x", "--database-url", "postgres:///c"]).unwrap();
        assert_eq!(
            (
                defaults.worlds.access_max_wallets,
                defaults.worlds.access_max_communities
            ),
            (1000, 50)
        );
        assert_eq!(
            with("--access-max-wallets", "10000")
                .unwrap()
                .worlds
                .access_max_wallets,
            10_000
        );
        assert_eq!(
            with("--access-max-communities", "1000")
                .unwrap()
                .worlds
                .access_max_communities,
            1_000
        );
        for (flag, over) in [
            ("--access-max-wallets", "10001"),
            ("--access-max-communities", "1001"),
        ] {
            assert!(with(flag, "0").is_err(), "{flag} 0");
            assert!(with(flag, over).is_err(), "{flag} {over}");
        }
    }

    #[test]
    fn worlds_defaults_are_the_old_binaries_values_and_writes_are_off() {
        let cli = parse(&["x", "--database-url", "postgres:///catalyst"]).unwrap();
        let c = cli;
        assert!(
            !c.worlds.worlds_writes_enabled,
            "worlds writes are opt-in; a restore must be demonstrated first"
        );
        assert!(
            c.worlds.worlds_forward_upstream_url.is_none(),
            "nothing is forwarded until an operator names an upstream"
        );
        assert!(c.worlds.catalyrst_worlds_admin_token.is_none());
        assert_eq!(
            (
                c.worlds.worlds_personal_worlds_max,
                c.worlds.worlds_personal_world_max_size_bytes
            ),
            (0, 52_428_800)
        );
        assert_eq!(
            (
                c.worlds.max_in_flight_upload_bytes,
                c.worlds.max_concurrent_uploads,
                c.worlds.max_in_flight_upload_files,
                c.worlds.multipart_upload_timeout_ms,
                c.worlds.deployment_processing_timeout_ms
            ),
            (4_294_967_296, 40, 40_000, 300_000, 300_000)
        );
        let w = &c.worlds;
        assert_eq!(
            (
                c.content.pending_deployment_ttl_ms,
                w.partial_upload_cleanup_interval_ms,
                c.content.max_pending_deployments_per_deployer,
                w.max_pending_deployments_per_world,
                w.pending_bytes_per_world_factor,
                c.content.max_pending_bytes_per_deployer,
                w.max_pending_bytes_per_source,
                c.content.max_pending_bytes,
                c.content.max_partial_upload_bytes_per_minute,
                w.completed_upload_ttl
            ),
            (
                3_600_000,
                300_000,
                10,
                4,
                2,
                1_073_741_824,
                4_294_967_296,
                53_687_091_200,
                536_870_912,
                86_400_000
            ),
            "worlds-content-server's .env.default"
        );
        assert!(c.worlds.encryption_key.is_none());
        assert_eq!(c.worlds.authorized_addresses, "");
        assert_eq!(c.worlds.storage_cache_enabled, None);
        assert_eq!(
            (
                c.worlds.storage_cache_ttl_seconds,
                c.worlds.storage_cache_max,
                c.worlds.storage_cache_max_value_bytes
            ),
            (60, 8_000, 32_768)
        );
        assert_eq!(
            (
                c.worlds.env_storage_max_value_size_bytes,
                c.worlds.env_storage_max_total_size_bytes
            ),
            (10_240, 262_144)
        );
        assert_eq!(
            (
                c.worlds.world_storage_max_value_size_bytes,
                c.worlds.world_storage_max_total_size_bytes
            ),
            (524_288, 10_485_760)
        );
        assert_eq!(
            (
                c.worlds.player_storage_max_value_size_bytes,
                c.worlds.player_storage_max_total_size_bytes
            ),
            (102_400, 1_048_576)
        );
        assert_eq!(
            (
                c.worlds.export_max_concurrent_per_scene,
                c.worlds.export_max_concurrent_per_pod,
                c.worlds.export_scene_cooldown_seconds,
                c.worlds.export_timeout_seconds,
                c.worlds.export_max_size_bytes,
                c.worlds.pg_component_stream_query_timeout
            ),
            (1, 5, 10, 120, 524_288_000, 120_000)
        );
        assert!(
            !c.worlds.scene_host_enabled
                && !c.worlds.scene_host_disable_js_runtime
                && !c.worlds.scene_host_storage_allow_http
                && c.worlds.scene_host_autoload
        );
        assert!(
            c.worlds.scene_host_local_scene_path.is_none()
                && c.worlds.scene_host_debugging_secret.is_none()
                && c.worlds.scene_host_admin_token.is_none()
        );
        assert!(
            c.worlds.scene_host_storage_url.is_none()
                && c.worlds.scene_host_authoritative_key.is_none()
                && c.worlds.scene_host_storage_delegation.is_none()
        );
        assert!(
            c.worlds.scene_host_delegation_minter_url.is_none()
                && c.worlds.scene_host_delegation_minter_token.is_none()
        );
        assert_eq!(
            (
                c.worlds.scene_host_auth_timeout_secs,
                c.worlds.scene_host_delegation_ttl_secs,
                c.worlds.scene_host_max_scenes,
                c.worlds.scene_host_sweep_interval_secs,
                c.worlds.scene_host_idle_unload_secs
            ),
            (5, 3600, 64, 30, 300)
        );
        assert_eq!(
            (
                c.worlds.js_heap_limit_mb,
                c.worlds.js_tick_budget_ms,
                c.worlds.js_shutdown_join_ms,
                c.worlds.js_update_failure_cap
            ),
            (384, 250, 2000, 30)
        );
        assert_eq!(
            (
                c.worlds.client_outbound_max,
                c.worlds.client_inbound_max,
                c.worlds.crdt_max_components,
                c.worlds.ws_max_frame_bytes
            ),
            (1024, 1024, 100_000, 2_097_152)
        );
        assert_eq!(
            (
                c.worlds.crdt_max_value_bytes,
                c.worlds.crdt_max_bytes,
                c.worlds.client_inbound_max_bytes,
                c.worlds.scene_inbound_max_bytes,
                c.worlds.client_outbound_max_bytes,
                c.worlds.js_array_buffer_limit_mb
            ),
            (262_144, 33_554_432, 8_388_608, 33_554_432, 8_388_608, 128)
        );
        assert_eq!(
            (
                c.worlds.signed_fetch_max_response_bytes,
                c.worlds.signed_fetch_max_body_bytes,
                c.worlds.signed_fetch_max_in_flight,
                c.worlds.signed_fetch_timeout_ms
            ),
            (2_097_152, 1_048_576, 8, 10_000)
        );
        assert_eq!(
            (c.worlds.handshake_timeout, c.worlds.island_changed_dedup_ms),
            (60_000, 10_000.0)
        );
        assert!(
            c.worlds.deny_list().is_none()
                && c.worlds.nats_url.is_none()
                && c.worlds.archipelago_control_v4_audience.is_none()
        );
        assert!(
            c.worlds_fed.worlds_fed_peers_file.is_none()
                && !c.worlds_fed.worlds_fed_allow_insecure_loopback_peers
        );
        assert_eq!(
            (
                c.worlds_fed.worlds_fed_poll_interval_secs,
                c.worlds_fed.worlds_fed_max_response_bytes,
                c.worlds_fed.worlds_fed_max_worlds_per_peer
            ),
            (300, 4 * 1024 * 1024, 10_000)
        );

        let cli = parse(&[
            "x",
            "--database-url",
            "postgres:///catalyst",
            "--worlds-writes-enabled",
            "true",
            "--scene-host-enabled",
            "true",
            "--worlds-personal-worlds-max",
            "3",
            "--nats-url",
            "nats://127.0.0.1:4222",
        ])
        .unwrap();
        assert!(cli.worlds.worlds_writes_enabled && cli.worlds.scene_host_enabled);
        assert_eq!(cli.worlds.worlds_personal_worlds_max, 3);
        assert_eq!(
            cli.worlds.nats_url.as_deref(),
            Some("nats://127.0.0.1:4222")
        );
    }
}
