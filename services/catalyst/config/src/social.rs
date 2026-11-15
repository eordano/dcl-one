//! The social plane's settings and the Pulse room authority.

use clap::Args;

// @well-known-components/env-config-provider getNumber uses parseFloat,
// including numeric prefixes and Infinity, and rejects NaN.
fn source_number(raw: &str) -> Result<f64, String> {
    let raw =
        raw.trim_start_matches(|c: char| c == '\u{feff}' || (c != '\u{85}' && c.is_whitespace()));
    let bytes = raw.as_bytes();
    let mut end = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    if raw
        .get(end..)
        .is_some_and(|suffix| suffix.starts_with("Infinity"))
    {
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
        return Err("Value is not a number".into());
    }
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let exponent = end;
        end += 1;
        if matches!(bytes.get(end), Some(b'+' | b'-')) {
            end += 1;
        }
        let start = end;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        if end == start {
            end = exponent;
        }
    }
    raw[..end]
        .parse::<f64>()
        .map_err(|_| "Value is not a number".into())
}

// uWS's binding converts through Int32Value into an unsigned-short field.
// The native context then accepts zero (disabled), or 8..=960 seconds.
fn source_idle_timeout(raw: &str) -> Result<u64, String> {
    let number = source_number(raw)?;
    let seconds = if number.is_finite() {
        number.trunc().rem_euclid(65536.0) as u64
    } else {
        0
    };
    if seconds == 0 || (8..=960).contains(&seconds) {
        Ok(seconds)
    } else {
        Err("idle timeout must resolve to zero or 8..=960 seconds".into())
    }
}

fn source_body_limit(raw: &str) -> Result<usize, String> {
    let value=source_number(raw).map_err(|_|format!("Configuration: config \"HTTP_MAX_REQUEST_BODY_BYTES\" should be a number, got string ({raw}) instead"))?;
    if !value.is_finite() || value.fract() != 0.0 || !(1.0..=16_777_216.0).contains(&value) {
        let display = if value == 0.0 {
            "0".to_owned()
        } else if value.is_infinite() {
            if value.is_sign_negative() {
                "-Infinity".to_owned()
            } else {
                "Infinity".to_owned()
            }
        } else if value.abs() >= 1e21 || value.abs() < 1e-6 {
            let scientific = format!("{value:e}");
            let (mantissa, exponent) = scientific.split_once('e').unwrap();
            let exponent: i32 = exponent.parse().unwrap();
            format!("{mantissa}e{exponent:+}")
        } else {
            value.to_string()
        };
        return Err(format!(
            "Invalid HTTP_MAX_REQUEST_BODY_BYTES: expected an integer number of bytes between 1 and 16777216, got {display}"
        ));
    }
    Ok(value as usize)
}

/// Upstream's presence badge defaults (the badges processor's environment),
/// shared with `badges::presence::Settings::default()`.
pub mod presence_badges {
    pub const GRANT_NOTIFICATIONS: bool = true;
    pub const EARLY_JOIN_LOOKAHEAD_SECONDS: u64 = 21_600;
    pub const OPEN_VISIT_CHECK_AFTER_SECONDS: u64 = 900;
    pub const OPEN_VISIT_MAX_AGE_SECONDS: u64 = 86_400;
    pub const APPLIED_EVENTS_TTL_SECONDS: u64 = 604_800;
    pub const ABSENT_EXPIRE_AFTER_SECONDS: u64 = 3_600;
    pub const SWEEP_INTERVAL_SECONDS: u64 = 60;
    pub const SWEEP_BATCH_SIZE: u32 = 100;
    pub const PRUNE_BATCH_SIZE: u32 = 1_000;
    pub const SWEEPER_ENABLED: bool = true;
    /// The longest a presence duration setting may be (30 days).
    pub const MAX_SECONDS: u64 = 2_592_000;
    pub const MAX_BATCH_SIZE: i64 = 100_000;
}

/// Social: communities, the social RPC, comms, notifications, badges, media and profile images.
#[derive(Args, Clone)]
pub struct SocialConfig {
    /// Request-body cap for the social-api gateway, validated before serving.
    #[arg(long,env="HTTP_MAX_REQUEST_BODY_BYTES",default_value="1048576",value_parser=source_body_limit,allow_hyphen_values=true)]
    pub social_http_max_request_body_bytes: usize,

    /// Bearer of the community admin routes (old API_ADMIN_TOKEN); unset refuses them.
    #[arg(long, env = "COMMUNITIES_ADMIN_TOKEN", hide_env_values = true)]
    pub communities_admin_token: Option<String>,

    /// Comma-separated wallets allowed on GET /v1/moderation/communities.
    #[arg(long, env = "COMMUNITIES_GLOBAL_MODERATORS", default_value = "")]
    pub communities_global_moderators: String,

    /// Feature service base; `dapps.json` is fetched with SERVICE_BASE_URL as Referer.
    #[arg(long, env = "FF_URL", default_value = "")]
    pub social_feature_flags_url: String,

    #[arg(
        long,
        env = "SERVICE_BASE_URL",
        default_value = "https://social-service-ea.decentraland.zone"
    )]
    pub social_service_base_url: String,

    #[arg(
        long,
        env = "FEATURE_FLAG_REFRESH_INTERVAL_IN_MS",
        default_value_t = 240_000
    )]
    pub social_feature_flags_refresh_ms: u64,

    #[arg(long, env = "FF_REQUEST_TIMEOUT", default_value_t = 10_000)]
    pub social_feature_flags_timeout_ms: u64,

    #[arg(long, env = "FF_DAPPS_COMMUNITIES_AI_COMPLIANCE")]
    pub social_ai_compliance_override: Option<String>,

    #[arg(long, env = "FF_DAPPS_DEV_COMMUNITIES_AI_COMPLIANCE")]
    pub social_dev_ai_compliance_override: Option<String>,

    #[arg(long, env = "FF_DAPPS_COMMUNITIES_GLOBAL_MODERATORS")]
    pub social_global_moderators_override: Option<String>,

    #[arg(long, env = "FF_DAPPS_REFERRAL_REGISTRATION_DISABLED")]
    pub social_referral_registration_disabled_override: Option<String>,

    /// Source environment for AI compliance and the Slack development channel.
    #[arg(long, env = "ENV")]
    pub social_environment: Option<String>,

    #[arg(long, env = "ANALYTICS_CONTEXT")]
    pub social_analytics_context: Option<String>,

    #[arg(long, env = "ANALYTICS_API_URL")]
    pub social_analytics_api_url: Option<String>,

    /// Also readable from ANALYTICS_API_TOKEN_FILE; never accepted on argv.
    #[arg(
        id = "analytics_api_token",
        long = "analytics-api-token",
        env = "ANALYTICS_API_TOKEN",
        hide_env_values = true
    )]
    pub social_analytics_api_token: Option<String>,

    #[arg(long, env = "ANALYTICS_REQUEST_TIMEOUT", value_parser = source_number, allow_hyphen_values = true)]
    pub social_analytics_request_timeout_ms: Option<f64>,

    #[arg(long, env = "OPEN_AI_MODEL", default_value = "gpt-5-nano")]
    pub community_compliance_model: String,

    /// Provider credential, also readable from OPEN_AI_API_KEY_FILE.
    #[arg(long, env = "OPEN_AI_API_KEY", hide_env_values = true)]
    pub community_compliance_api_key: Option<String>,

    #[arg(
        long,
        env = "OPENAI_BASE_URL",
        default_value = "https://api.openai.com/v1"
    )]
    pub community_compliance_base_url: String,

    #[arg(long, env = "OPENAI_ORG_ID")]
    pub community_compliance_organization: Option<String>,

    #[arg(long, env = "OPENAI_PROJECT_ID")]
    pub community_compliance_project: Option<String>,

    /// Comma-separated community names refused on create/rename (folded comparison).
    #[arg(long, env = "RESTRICTED_NAMES", default_value = "")]
    pub restricted_names: String,

    /// Base of community thumbnail URLs (old CDN_URL); default PUBLIC_URL.
    #[arg(long, env = "COMMUNITIES_CDN_URL")]
    pub communities_cdn_url: Option<String>,

    /// Archipelago stats base URL for the onlyOnline member filter; unset renders everyone offline.
    #[arg(long, env = "ARCHIPELAGO_STATS_URL")]
    pub archipelago_stats_url: Option<String>,

    /// Require places added to a community to be owned by the caller per the mirrored places catalogue
    /// (on by default, as upstream always checks; `false` skips the check).
    #[arg(long, env = "COMMUNITIES_PLACES_OWNERSHIP_CHECK", default_value_t = true, action = clap::ArgAction::Set)]
    pub communities_places_ownership_check: bool,

    /// Mirror the public community listing from `COMMUNITIES_UPSTREAM_URL`
    /// (`social::communities::mirror`): read-only copies, no local roles.
    #[arg(long, env = "COMMUNITIES_MIRROR_UPSTREAM", default_value_t = false, action = clap::ArgAction::Set)]
    pub communities_mirror_upstream: bool,

    /// The social API the listing mirrors from (the mirror posture's).
    #[arg(long, env = "COMMUNITIES_UPSTREAM_URL", default_value = "")]
    pub communities_upstream_url: String,

    /// The base the upstream serves community thumbnails under (the mirror
    /// posture's).
    #[arg(long, env = "COMMUNITIES_MIRROR_THUMBNAIL_URL", default_value = "")]
    pub communities_mirror_thumbnail_url: String,

    #[arg(long, env = "COMMUNITIES_MIRROR_INTERVAL_SECS", default_value_t = 3600)]
    pub communities_mirror_interval_secs: u64,

    /// Friendship/block fixed-window limits, shared by every serving replica.
    #[arg(
        long,
        env = "FRIENDSHIP_RATE_LIMIT_WINDOW_SECONDS",
        default_value_t = 60.0,
        value_parser = source_number,
        allow_hyphen_values = true
    )]
    pub friendship_rate_limit_window_seconds: f64,
    #[arg(
        long,
        env = "FRIENDSHIP_RATE_LIMIT_PER_ACTOR",
        default_value_t = 30.0,
        value_parser = source_number,
        allow_hyphen_values = true
    )]
    pub friendship_rate_limit_per_actor: f64,
    #[arg(
        long,
        env = "FRIENDSHIP_RATE_LIMIT_PER_PAIR",
        default_value_t = 10.0,
        value_parser = source_number,
        allow_hyphen_values = true
    )]
    pub friendship_rate_limit_per_pair: f64,

    /// Optional source Slack producer; supports SLACK_BOT_TOKEN_FILE.
    #[arg(long, env = "SLACK_BOT_TOKEN", hide_env_values = true)]
    pub referral_slack_token: Option<String>,
    #[arg(long, env = "REFERRAL_METABASE_DASHBOARD", default_value = "")]
    pub referral_metabase_dashboard: String,
    #[arg(long, env = "REFERRAL_FIVE_MINUTES_IN_MS", default_value_t = 300_000.0,
        value_parser = source_number, allow_hyphen_values = true)]
    pub referral_suspicious_window_ms: f64,
    #[arg(long, env = "REFERRAL_MAX_IP_MATCHES", default_value_t = 2.0,
        value_parser = source_number, allow_hyphen_values = true)]
    pub referral_max_ip_matches: f64,
    #[arg(long, env = "REFERRAL_MIN_LOGIN_DAYS", default_value_t = 3.0,
        value_parser = source_number, allow_hyphen_values = true)]
    pub referral_min_login_days: f64,
    #[arg(long, env = "REWARD_SERVER_URL")]
    pub referral_reward_server_url: Option<String>,
    #[arg(
        long,
        env = "REWARDS_API_KEY_BY_REFERRAL_INVITED_USERS_5",
        hide_env_values = true
    )]
    pub rewards_api_key_by_referral_invited_users_5: Option<String>,
    #[arg(
        long,
        env = "REWARDS_API_KEY_BY_REFERRAL_INVITED_USERS_10",
        hide_env_values = true
    )]
    pub rewards_api_key_by_referral_invited_users_10: Option<String>,
    #[arg(
        long,
        env = "REWARDS_API_KEY_BY_REFERRAL_INVITED_USERS_20",
        hide_env_values = true
    )]
    pub rewards_api_key_by_referral_invited_users_20: Option<String>,
    #[arg(
        long,
        env = "REWARDS_API_KEY_BY_REFERRAL_INVITED_USERS_25",
        hide_env_values = true
    )]
    pub rewards_api_key_by_referral_invited_users_25: Option<String>,
    #[arg(
        long,
        env = "REWARDS_API_KEY_BY_REFERRAL_INVITED_USERS_30",
        hide_env_values = true
    )]
    pub rewards_api_key_by_referral_invited_users_30: Option<String>,
    #[arg(
        long,
        env = "REWARDS_API_KEY_BY_REFERRAL_INVITED_USERS_50",
        hide_env_values = true
    )]
    pub rewards_api_key_by_referral_invited_users_50: Option<String>,
    #[arg(
        long,
        env = "REWARDS_API_KEY_BY_REFERRAL_INVITED_USERS_60",
        hide_env_values = true
    )]
    pub rewards_api_key_by_referral_invited_users_60: Option<String>,
    #[arg(
        long,
        env = "REWARDS_API_KEY_BY_REFERRAL_INVITED_USERS_75",
        hide_env_values = true
    )]
    pub rewards_api_key_by_referral_invited_users_75: Option<String>,

    #[arg(long, env = "PROFILE_URL", default_value = "https://decentraland.org")]
    pub referral_profile_url: String,

    /// Handshake frame deadline and timestamp window, seconds.
    #[arg(long, env = "AUTH_WINDOW_SECS", default_value_t = 300)]
    pub social_rpc_auth_window_secs: i64,

    /// First-frame authentication deadline; independent of signature age.
    #[arg(long, env = "WS_AUTH_TIMEOUT_IN_MS", value_parser = source_number, allow_hyphen_values = true)]
    pub ws_auth_timeout_in_ms: Option<f64>,
    /// Maximum absolute signature age in milliseconds.
    #[arg(long, env = "WS_AUTH_SIGNATURE_EXPIRATION_IN_MS", value_parser = source_number, allow_hyphen_values = true)]
    pub ws_auth_signature_expiration_in_ms: Option<f64>,
    /// Seconds without socket traffic; zero disables idle expiry.
    #[arg(long, env = "WS_IDLE_TIMEOUT_IN_SECONDS", default_value_t = 300, value_parser = source_idle_timeout, allow_hyphen_values = true)]
    pub ws_idle_timeout_in_seconds: u64,

    /// A private call not accepted within this many ms expires.
    #[arg(
        long,
        env = "PRIVATE_VOICE_CHAT_EXPIRATION_TIME",
        default_value_t = 60_000
    )]
    pub private_voice_chat_expiration_ms: i64,

    /// Tick of the private-call expiry sweep, ms.
    #[arg(long, env = "PRIVATE_VOICE_CHAT_JOB_INTERVAL", default_value_t = 1_000)]
    pub private_voice_chat_job_interval_ms: u64,

    /// Tick of the shared community voice notification status poll, milliseconds.
    #[arg(
        long,
        env = "COMMUNITY_VOICE_CHAT_POLLING_JOB_INTERVAL",
        default_value_t = 45_000
    )]
    pub community_voice_chat_polling_job_interval_ms: u64,

    /// Expired private calls reclaimed per sweep statement.
    #[arg(
        long,
        env = "PRIVATE_VOICE_CHAT_EXPIRATION_BATCH_SIZE",
        default_value_t = 20
    )]
    pub private_voice_chat_expiration_batch_size: i64,

    /// Concurrent social sockets; unset = unlimited (1013 past it).
    #[arg(long, env = "WS_MAX_CONCURRENT_CONNECTIONS")]
    pub ws_max_concurrent_connections: Option<usize>,

    /// Largest inbound WebSocket frame, bytes.
    #[arg(long, env = "WS_MAX_PAYLOAD_LENGTH", default_value_t = 1_048_576)]
    pub ws_max_payload_length: usize,

    /// Source number used by the ordinary RPC queue. Socket writers truncate
    /// and wrap it to an unsigned-32-bit byte threshold before enforcing it.
    #[arg(long, env = "WS_MAX_BACKPRESSURE", default_value_t = 131072.0, value_parser = source_number, allow_hyphen_values = true)]
    pub ws_max_backpressure: f64,

    /// Source ordinary RPC queue and retry settings.
    #[arg(long, env = "WS_TRANSPORT_MAX_QUEUE_SIZE", default_value_t = 1000.0, value_parser = source_number, allow_hyphen_values = true)]
    pub ws_transport_max_queue_size: f64,
    #[arg(long, env = "WS_TRANSPORT_MIN_QUEUE_SIZE", default_value_t = 100.0, value_parser = source_number, allow_hyphen_values = true)]
    pub ws_transport_min_queue_size: f64,
    #[arg(long, env = "WS_TRANSPORT_MAX_QUEUE_SIZE_LIMIT", default_value_t = 5000.0, value_parser = source_number, allow_hyphen_values = true)]
    pub ws_transport_max_queue_size_limit: f64,
    #[arg(long, env = "WS_TRANSPORT_ESTIMATED_MESSAGE_SIZE", default_value_t = 1024.0, value_parser = source_number, allow_hyphen_values = true)]
    pub ws_transport_estimated_message_size: f64,
    #[arg(long, env = "WS_TRANSPORT_RETRY_DELAY_MS", default_value_t = 1000.0, value_parser = source_number, allow_hyphen_values = true)]
    pub ws_transport_retry_delay_ms: f64,
    #[arg(long, env = "WS_TRANSPORT_MAX_RETRY_ATTEMPTS", default_value_t = 5.0, value_parser = source_number, allow_hyphen_values = true)]
    pub ws_transport_max_retry_attempts: f64,
    #[arg(long, env = "WS_TRANSPORT_MAX_BACKOFF_DELAY_MS", default_value_t = 30000.0, value_parser = source_number, allow_hyphen_values = true)]
    pub ws_transport_max_backoff_delay_ms: f64,

    /// Authenticated social sockets per address across both protocols; past
    /// it the address's oldest closes 3005.
    #[arg(long, env = "WS_MAX_CONNECTIONS_PER_ADDRESS", default_value_t = 8, value_parser = clap::value_parser!(u32).range(1..))]
    pub ws_max_connections_per_address: u32,

    /// Stream subscriptions an address may start per second, refilling its bucket.
    #[arg(long, env = "WS_SUBSCRIPTIONS_PER_SECOND", default_value_t = 2.0, value_parser = positive)]
    pub ws_subscriptions_per_second: f64,

    /// The size of an address's subscription bucket.
    #[arg(long, env = "WS_SUBSCRIPTION_BURST", default_value_t = 24, value_parser = clap::value_parser!(u32).range(1..))]
    pub ws_subscription_burst: u32,

    /// Disconnected addresses whose partly spent subscription bucket is remembered.
    #[arg(long, env = "WS_ADDRESS_BUDGET_CAPACITY", default_value_t = 10_000, value_parser = clap::value_parser!(u32).range(1..))]
    pub ws_address_budget_capacity: u32,

    /// Largest inbound /social-rpc/v1/stream frame, bytes.
    #[arg(long, env = "STREAM_WS_MAX_PAYLOAD_LENGTH", default_value_t = 16_384, value_parser = clap::value_parser!(u32).range(1..))]
    pub stream_ws_max_payload_length: u32,

    /// Frames a /social-rpc/v1/stream socket may send per second, refilling its bucket.
    #[arg(long, env = "STREAM_WS_MESSAGES_PER_SECOND", default_value_t = 20.0, value_parser = positive)]
    pub stream_ws_messages_per_second: f64,

    /// The size of a /social-rpc/v1/stream socket's frame bucket.
    #[arg(long, env = "STREAM_WS_MESSAGE_BURST", default_value_t = 40, value_parser = clap::value_parser!(u32).range(1..))]
    pub stream_ws_message_burst: u32,

    /// Bearer of the /admin/social routes; unset refuses them.
    #[arg(long, env = "CATALYRST_SOCIAL_RPC_ADMIN_TOKEN", hide_env_values = true)]
    pub catalyrst_social_rpc_admin_token: Option<String>,

    /// Source CURRENT_VERSION string returned verbatim by the social RPC and notifications /status.
    #[arg(long, env = "CURRENT_VERSION")]
    pub social_rpc_current_version: Option<String>,

    /// Legacy uWS metrics path; unset/empty uses /metrics under /social-rpc.
    #[arg(long, env = "WKC_METRICS_PUBLIC_PATH")]
    pub wkc_metrics_public_path: Option<String>,

    /// Legacy metrics Bearer; unset/empty permits scrapes.
    #[arg(long, env = "WKC_METRICS_BEARER_TOKEN", hide_env_values = true)]
    pub wkc_metrics_bearer_token: Option<String>,

    /// Source opt-in reset flag. Shared-recorder reset parity remains a known gap.
    #[arg(long, env = "WKC_METRICS_RESET_AT_NIGHT")]
    pub wkc_metrics_reset_at_night: Option<String>,

    // comms::gatekeeper: the comms gatekeeper (LiveKit token minting, scene and
    // platform moderation, voice presence). The LiveKit media server is a
    // separate process reached over the network.
    /// LiveKit host; signaling and API URLs derive from it.
    #[arg(long, env = "LIVEKIT_HOST", default_value = "livekit.local")]
    pub livekit_host: String,

    /// LiveKit API base when it differs from the host.
    #[arg(long, env = "LIVEKIT_API_HOST")]
    pub livekit_api_host: Option<String>,

    /// Client-facing LiveKit signaling URL when it differs from the host.
    #[arg(long, env = "LIVEKIT_WS_URL")]
    pub livekit_ws_url: Option<String>,

    /// LiveKit API key (or LIVEKIT_API_KEY_FILE); required unless LIVEKIT_ALLOW_DEV_CREDS.
    #[arg(long, env = "LIVEKIT_API_KEY", hide_env_values = true)]
    pub livekit_api_key: Option<String>,

    /// LiveKit API secret (or LIVEKIT_API_SECRET_FILE).
    #[arg(long, env = "LIVEKIT_API_SECRET", hide_env_values = true)]
    pub livekit_api_secret: Option<String>,

    /// Start on the devkey/devsecret placeholders (tokens parse locally, no real SFU accepts them).
    #[arg(long, env = "LIVEKIT_ALLOW_DEV_CREDS", default_value_t = false, action = clap::ArgAction::Set)]
    pub livekit_allow_dev_creds: bool,

    #[arg(
        long,
        env = "PRIVATE_MESSAGES_ROOM_ID",
        default_value = "private-messages"
    )]
    pub private_messages_room_id: String,

    /// The one wallet that may mint the authoritative-server host token; unset refuses /get-server-scene-adapter.
    #[arg(long, env = "AUTHORITATIVE_SERVER_ADDRESS")]
    pub authoritative_server_address: Option<String>,

    /// Bearer of the platform user-moderation routes.
    #[arg(long, env = "MODERATOR_TOKEN", hide_env_values = true)]
    pub moderator_token: Option<String>,

    /// Comma/space-separated wallets allowed on the user-moderation routes by signed fetch.
    #[arg(long, env = "PLATFORM_USER_MODERATORS", default_value = "")]
    pub platform_user_moderators: String,

    /// Report evidence the node accepts in 24 hours from all wallets and
    /// addresses, in MiB of declared size: the files it holds and, for a
    /// quarter hour each, the files a presign declared. At the cap a presign
    /// is refused and 0 accepts none; one client address takes at most a
    /// sixteenth of it. The default is a policy, not a measurement: it
    /// is how much store space the operator gives to reports a day, and no
    /// load test or disk size produced it.
    #[arg(long, env = "REPORT_EVIDENCE_DAILY_MIB", default_value_t = 2048)]
    pub report_evidence_daily_mib: u32,

    /// Service token in front of the voice routes, the DM privacy patch and
    /// the world ban status; unset answers 503 on them.
    #[arg(long, env = "COMMS_GATEKEEPER_AUTH_TOKEN", hide_env_values = true)]
    pub comms_gatekeeper_auth_token: Option<String>,

    /// Lambdas base whose `/parcels/{x}/{y}/operators` protects LAND
    /// owners/operators from scene bans (old LAMBDAS_URL); unset protects none.
    #[arg(long, env = "COMMS_LAND_OPERATORS_URL")]
    pub comms_land_operators_url: Option<String>,

    /// The linker-server lease document (`[{addresses, plots}]`) whose
    /// holders administer the scenes on their plots (the mirror posture's:
    /// Decentraland's published list); empty lists no lease holder.
    #[arg(long, env = "LEASE_AUTHORIZATIONS_URL", default_value = "")]
    pub lease_authorizations_url: String,

    /// This catalyst's federation identity, the epoch author of the MLS
    /// groups created here (normally its public domain); unset uses the id
    /// `social.fed_instance_identity` minted at schema apply.
    #[arg(long, env = "FED_PEER_ID")]
    pub fed_peer_id: Option<String>,

    #[arg(
        long,
        env = "VOICE_CHAT_CONNECTION_INTERRUPTED_TTL",
        default_value_t = 300_000
    )]
    pub voice_chat_connection_interrupted_ttl_ms: i64,

    #[arg(
        long,
        env = "VOICE_CHAT_INITIAL_CONNECTION_TTL",
        default_value_t = 300_000
    )]
    pub voice_chat_initial_connection_ttl_ms: i64,

    #[arg(
        long,
        env = "COMMUNITY_VOICE_CHAT_NO_MODERATOR_TTL",
        default_value_t = 300_000
    )]
    pub community_voice_chat_no_moderator_ttl_ms: i64,

    #[arg(long, env = "VOICE_CHAT_EXPIRED_BATCH_SIZE", default_value_t = 50)]
    pub voice_chat_expired_batch_size: i64,

    /// Bearer of POST /notifications/notifications/broadcast; unset refuses it.
    #[arg(
        long,
        env = "CATALYRST_NOTIFICATIONS_ADMIN_TOKEN",
        hide_env_values = true
    )]
    pub catalyrst_notifications_admin_token: Option<String>,

    #[arg(long, env = "SENDGRID_API_KEY", hide_env_values = true)]
    pub sendgrid_api_key: Option<String>,

    /// SendGrid's API origin; `/v3/mail/send` is appended. Empty means no SendGrid.
    #[arg(
        long,
        env = "SENDGRID_API_URL",
        default_value = "https://api.sendgrid.com"
    )]
    pub sendgrid_api_url: String,

    /// Read through [`SocialConfig::sendgrid_sender`], which falls back to `SENDGRID_EMAIL_FROM`.
    #[arg(long, env = "SENDGRID_FROM_EMAIL")]
    pub sendgrid_from_email: Option<String>,

    /// Upstream's name for SENDGRID_FROM_EMAIL, used when that is unset.
    #[arg(long, env = "SENDGRID_EMAIL_FROM")]
    pub sendgrid_email_from: Option<String>,

    /// The generic template: outbox deliveries, confirmations and ad-hoc emails; unset holds every delivery pending.
    /// Read through [`SocialConfig::sendgrid_template_id`], which falls back to `SENDGRID_EMAIL_TEMPLATE_ID`.
    #[arg(long, env = "SENDGRID_NOTIFICATION_TEMPLATE_ID")]
    pub sendgrid_notification_template_id: Option<String>,

    /// Upstream's name for SENDGRID_NOTIFICATION_TEMPLATE_ID, used when that is unset.
    #[arg(long, env = "SENDGRID_EMAIL_TEMPLATE_ID")]
    pub sendgrid_email_template_id: Option<String>,

    /// The account confirmation email's own template; unset uses SENDGRID_NOTIFICATION_TEMPLATE_ID.
    #[arg(long, env = "SENDGRID_VALIDATE_EMAIL_TEMPLATE_ID")]
    pub sendgrid_validate_email_template_id: Option<String>,

    /// The credits confirmation email's own template; unset uses SENDGRID_NOTIFICATION_TEMPLATE_ID.
    #[arg(long, env = "SENDGRID_VALIDATE_CREDITS_EMAIL_TEMPLATE_ID")]
    pub sendgrid_validate_credits_email_template_id: Option<String>,

    /// SendGrid validates notification, confirmation and ad-hoc emails but delivers none (never the code-rendered mail).
    #[arg(long, env = "SENDGRID_SANDBOX_MODE", default_value_t = false, action = clap::ArgAction::Set)]
    pub sendgrid_sandbox_mode: bool,

    /// HMAC key of the notification emails' unsubscribe links; unset holds notification emails pending.
    #[arg(long, env = "SIGNING_KEY", hide_env_values = true)]
    pub signing_key: Option<String>,

    /// Public notifications origin of the email unsubscribe links.
    #[arg(
        long,
        env = "NOTIFICATIONS_SERVICE_BASE_URL",
        default_value = "https://notifications.decentraland.org"
    )]
    pub notifications_service_base_url: String,

    #[arg(
        long,
        env = "ACCOUNT_BASE_URL",
        default_value = "https://account.decentraland.org"
    )]
    pub account_base_url: String,

    #[arg(
        long,
        env = "MARKETPLACE_BASE_URL",
        default_value = "https://decentraland.org/marketplace"
    )]
    pub marketplace_base_url: String,

    /// Bearer of POST /notifications/notifications/email; unset refuses it.
    #[arg(long, env = "NOTIFICATION_SERVICE_TOKEN", hide_env_values = true)]
    pub notification_service_token: Option<String>,

    /// Governance's bearer of POST /notifications/notifications (`governance_*` types only); unset disables it.
    #[arg(long, env = "NOTIFICATIONS_GOVERNANCE_TOKEN", hide_env_values = true)]
    pub notifications_governance_token: Option<String>,

    /// The rewards app's bearer of POST /notifications/notifications (`reward_*` types only); unset disables it.
    #[arg(long, env = "NOTIFICATIONS_REWARDS_TOKEN", hide_env_values = true)]
    pub notifications_rewards_token: Option<String>,

    /// Cloudflare Turnstile secret for confirm-email; the check runs while it is set and the
    /// `dapps-turnstile-verification` flag is on.
    #[arg(long, env = "TURNSTILE_SECRET_KEY", hide_env_values = true)]
    pub turnstile_secret_key: Option<String>,

    /// The disposable email domain list set-email and confirm-email refuse, fetched hourly; empty
    /// (the default outside the mirror posture) fetches none.
    #[arg(long, env = "DISPOSABLE_EMAIL_DOMAINS_URL", default_value = "")]
    pub disposable_email_domains_url: String,

    /// A disposable list download with fewer domains is treated as broken and the previous list
    /// kept. The upstream list holds thousands; lower it for a shorter list of your own.
    #[arg(long, env = "DISPOSABLE_EMAIL_DOMAINS_MIN", default_value_t = 1000)]
    pub disposable_email_domains_min: usize,

    /// Comma-separated email domains refused by set-email and confirm-email, on top of the
    /// disposable-domain list and the `dapps-credits-blacklisted-domains` flag variant.
    #[arg(long, env = "EMAIL_DOMAIN_BLACKLIST", default_value = "")]
    pub email_domain_blacklist: String,

    /// Run the friend_first_wear producer over the synced profiles.
    #[arg(long, env = "FIRST_WEAR_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub first_wear_enabled: bool,

    #[arg(
        long,
        env = "SHOP_ITEM_BASE_URL",
        default_value = "https://decentraland.org/marketplace/"
    )]
    pub shop_item_base_url: String,

    #[arg(long, env = "FIRST_WEAR_IMAGE_BASE")]
    pub first_wear_image_base: Option<String>,

    /// Base of the notification icons (upstream `CDN_URL`), e.g. `<base>/credits/notification-icon.png`.
    #[arg(
        long,
        env = "NOTIFICATIONS_CDN_URL",
        default_value = "https://assets-cdn.decentraland.org"
    )]
    pub notifications_cdn_url: String,

    /// Tick of the notification outbox worker.
    #[arg(
        long,
        env = "NOTIFICATION_DELIVERY_INTERVAL_SECS",
        default_value_t = 15
    )]
    pub notification_delivery_interval_secs: u64,

    /// Days a producer broadcast stays in every inbox before it is hidden and pruned; 0 keeps them forever.
    #[arg(
        long,
        env = "NOTIFICATIONS_BROADCAST_HORIZON_DAYS",
        default_value_t = 0
    )]
    pub notifications_broadcast_horizon_days: u64,
    /// Pending notification emails idle this long end skipped as expired (default 7 days;
    /// `0` never expires while SIGNING_KEY is set, other values below an hour are raised to an hour).
    #[arg(
        long,
        env = "NOTIFICATION_DELIVERY_MAX_AGE_SECS",
        default_value_t = 604_800
    )]
    pub notification_delivery_max_age_secs: u64,

    // social/badges.
    /// Bearer of the badge admin routes (read, grant, revoke); unset refuses them.
    #[arg(long, env = "CATALYRST_BADGES_ADMIN_TOKEN", hide_env_values = true)]
    pub catalyrst_badges_admin_token: Option<String>,

    /// The base every badge asset path resolves against: upstream's CDN, so
    /// the URLs are upstream's. `<node>/badges/assets` serves BADGES_ASSETS_DIR.
    #[arg(
        long,
        env = "BADGES_PUBLIC_ASSET_BASE_URL",
        default_value = "https://assets-cdn.decentraland.org"
    )]
    pub badges_public_asset_base_url: String,

    /// Badge art mirrored from upstream's CDN (`<badge>[/<tier>]/{2d,3d}/...`),
    /// served at /badges/assets.
    #[arg(long, env = "BADGES_ASSETS_DIR")]
    pub badges_assets_dir: Option<String>,

    /// Run the badge processor (leased: the primary only, and only while
    /// social writes are enabled): it reads social.client_events,
    /// content.deployments, the Polygon shop logs and mirrors and the accepted
    /// referrals, and grants upstream's evaluated badges with their
    /// badge_granted notifications. Off by default: a node opts in once its
    /// sources are current (the cursors start at the head when it does).
    #[arg(long, env = "BADGES_PROCESSOR_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub badges_processor_enabled: bool,

    /// Rows (or recounted wallets) the badge processor writes per transaction.
    #[arg(long, env = "BADGES_PROCESSOR_BATCH_SIZE", default_value_t = 200)]
    pub badges_processor_batch_size: u32,

    /// Upstream's EVENT_PROCESSING_MAX_AGE_IN_SECONDS: the badge processor
    /// passes, without effect, an event (client event, deployment, sale)
    /// older than this many seconds by its own time; 0 keeps every event.
    #[arg(long, env = "BADGES_EVENT_MAX_AGE_SECS", default_value_t = 0)]
    pub badges_event_max_age_secs: u64,

    /// A deployment the node stored more than this many seconds after its
    /// entity timestamp is history a syncing node caught up on: the badge
    /// processor passes it without effect; 0 (the default, as upstream had
    /// no such filter) keeps every deployment.
    #[arg(long, env = "BADGES_DEPLOYMENT_MAX_LAG_SECS", default_value_t = 0)]
    pub badges_deployment_max_lag_secs: u64,
    /// Count the gatekeeper's room joins and leaves toward the presence
    /// badges (social.presence_badges): every process stores the events,
    /// the leased badges-presence jobs apply them and close lost visits.
    #[arg(long, env = "BADGES_PRESENCE_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub badges_presence_enabled: bool,

    /// A presence grant writes its badge_granted notification; false
    /// grants without one.
    #[arg(long, env = "BADGES_PRESENCE_GRANT_NOTIFICATIONS", default_value_t = presence_badges::GRANT_NOTIFICATIONS, action = clap::ArgAction::Set)]
    pub badges_presence_grant_notifications: bool,

    /// A join this long before a badge's window opens is kept (0: none).
    #[arg(long, env = "BADGES_PRESENCE_EARLY_JOIN_LOOKAHEAD_SECONDS", default_value_t = presence_badges::EARLY_JOIN_LOOKAHEAD_SECONDS, value_parser = clap::value_parser!(u64).range(0..=presence_badges::MAX_SECONDS))]
    pub badges_presence_early_join_lookahead_seconds: u64,

    /// A visit open this long is checked against the LiveKit occupancy.
    #[arg(long, env = "BADGES_PRESENCE_OPEN_VISIT_CHECK_AFTER_SECONDS", default_value_t = presence_badges::OPEN_VISIT_CHECK_AFTER_SECONDS, value_parser = clap::value_parser!(u64).range(1..=presence_badges::MAX_SECONDS))]
    pub badges_presence_open_visit_check_after_seconds: u64,

    /// A connection open longer, or a join-to-leave stretch longer, proves no time.
    #[arg(long, env = "BADGES_PRESENCE_OPEN_VISIT_MAX_AGE_SECONDS", default_value_t = presence_badges::OPEN_VISIT_MAX_AGE_SECONDS, value_parser = clap::value_parser!(u64).range(1..=presence_badges::MAX_SECONDS))]
    pub badges_presence_open_visit_max_age_seconds: u64,

    /// How long applied events, pending leaves and absences are kept.
    #[arg(long, env = "BADGES_PRESENCE_APPLIED_EVENTS_TTL_SECONDS", default_value_t = presence_badges::APPLIED_EVENTS_TTL_SECONDS, value_parser = clap::value_parser!(u64).range(1..=presence_badges::MAX_SECONDS))]
    pub badges_presence_applied_events_ttl_seconds: u64,

    /// How long a visit unconfirmed by the occupancy waits for a late leave.
    #[arg(long, env = "BADGES_PRESENCE_ABSENT_EXPIRE_AFTER_SECONDS", default_value_t = presence_badges::ABSENT_EXPIRE_AFTER_SECONDS, value_parser = clap::value_parser!(u64).range(1..=presence_badges::MAX_SECONDS))]
    pub badges_presence_absent_expire_after_seconds: u64,

    /// Interval of the badges-presence-sweep job.
    #[arg(long, env = "BADGES_PRESENCE_SWEEP_INTERVAL_SECONDS", default_value_t = presence_badges::SWEEP_INTERVAL_SECONDS, value_parser = clap::value_parser!(u64).range(1..=presence_badges::MAX_SECONDS))]
    pub badges_presence_sweep_interval_seconds: u64,

    /// Visits the sweeper checks per pass.
    #[arg(long, env = "BADGES_PRESENCE_SWEEP_BATCH_SIZE", default_value_t = presence_badges::SWEEP_BATCH_SIZE, value_parser = clap::value_parser!(u32).range(1..=presence_badges::MAX_BATCH_SIZE))]
    pub badges_presence_sweep_batch_size: u32,

    /// Rows pruned per table per batch.
    #[arg(long, env = "BADGES_PRESENCE_PRUNE_BATCH_SIZE", default_value_t = presence_badges::PRUNE_BATCH_SIZE, value_parser = clap::value_parser!(u32).range(1..=presence_badges::MAX_BATCH_SIZE))]
    pub badges_presence_prune_batch_size: u32,

    /// false stops only the occupancy checks; the maximum age and the
    /// pruning still run.
    #[arg(long, env = "BADGES_PRESENCE_SWEEPER_ENABLED", default_value_t = presence_badges::SWEEPER_ENABLED, action = clap::ArgAction::Set)]
    pub badges_presence_sweeper_enabled: bool,

    // social/media: the autotranslate route (an external engine over the network).
    /// disabled | mock | http | llm; empty = http when TRANSLATE_BACKEND_URL
    /// is set, llm when TRANSLATE_LLM_BASE_URL is, else disabled (503).
    #[arg(long, env = "TRANSLATE_BACKEND", default_value = "")]
    pub translate_backend: String,

    #[arg(long, env = "TRANSLATE_BACKEND_URL", default_value = "")]
    pub translate_backend_url: String,

    #[arg(long, env = "TRANSLATE_BACKEND_API_KEY", hide_env_values = true)]
    pub translate_backend_api_key: Option<String>,

    #[arg(long, env = "TRANSLATE_LLM_BASE_URL", default_value = "")]
    pub translate_llm_base_url: String,

    #[arg(long, env = "TRANSLATE_LLM_API_KEY", hide_env_values = true)]
    pub translate_llm_api_key: Option<String>,

    #[arg(long, env = "TRANSLATE_LLM_MODEL", default_value = "gpt-4o-mini")]
    pub translate_llm_model: String,

    #[arg(long, env = "TRANSLATE_CHAR_LIMIT", default_value_t = 5000)]
    pub translate_char_limit: u64,

    #[arg(long, env = "TRANSLATE_BATCH_LIMIT", default_value_t = 100)]
    pub translate_batch_limit: u64,

    #[arg(long, env = "TRANSLATE_REQUEST_TIMEOUT_SECS", default_value_t = 30)]
    pub translate_request_timeout_secs: u64,

    /// Prune translation_cache rows older than this many days; 0 keeps them.
    #[arg(long, env = "TRANSLATE_CACHE_TTL_DAYS", default_value_t = 0)]
    pub translate_cache_ttl_days: u64,

    /// `/convert` fetches any public URL a client names (the mirror
    /// posture's); off, it converts only this node's own
    /// `<PUBLIC_URL>/content/contents/<hash>` files and fetches nothing.
    #[arg(long, env = "MEDIA_CONVERT_REMOTE_SOURCES", default_value_t = false, action = clap::ArgAction::Set)]
    pub media_convert_remote_sources: bool,

    // social/profile_images: avatar face/body images from the profile
    // entity's own files, the renderer and the upstream service as gated
    // fallbacks. Rendered and fetched images are blobs in the ONE store
    // (social.profile_images); this directory only holds render scratch.
    #[arg(
        long,
        env = "PROFILE_IMAGES_CACHE_DIR",
        default_value = "/var/lib/catalyst/profile-images"
    )]
    pub profile_images_cache_dir: String,

    #[arg(
        long,
        env = "PROFILE_IMAGES_CACHE_TTL_SECONDS",
        default_value_t = 86400
    )]
    pub profile_images_cache_ttl_seconds: u64,

    /// The rendered and fetched images' byte budget in the blob store; 0 = unbounded.
    #[arg(long, env = "PROFILE_IMAGES_CACHE_MAX_BYTES", default_value_t = 0)]
    pub profile_images_cache_max_bytes: u64,

    /// Upstream profile-images service, the explicit last resort; must be a public http(s) host.
    #[arg(long, env = "PROFILE_IMAGES_ORIGIN_URL", default_value = "")]
    pub profile_images_origin_url: String,

    #[arg(long, env = "PROFILE_IMAGES_RENDER_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub profile_images_render_enabled: bool,

    /// `bevy` (the engine's headless `avatar_render`) or `godot`.
    #[arg(long, env = "PROFILE_IMAGES_RENDERER", default_value = "bevy")]
    pub profile_images_renderer: String,

    /// The engine's `avatar_render` (`packages.batch-tools`); required with the bevy renderer.
    #[arg(long, env = "PROFILE_IMAGES_AVATAR_RENDER_BIN", default_value = "")]
    pub profile_images_avatar_render_bin: String,

    #[arg(long, env = "PROFILE_IMAGES_GODOT_BIN", default_value = "")]
    pub profile_images_godot_bin: String,

    #[arg(long, env = "PROFILE_IMAGES_GODOT_PROJECT", default_value = "")]
    pub profile_images_godot_project: String,

    #[arg(
        long,
        env = "PROFILE_IMAGES_RENDERING_METHOD",
        default_value = "gl_compatibility"
    )]
    pub profile_images_rendering_method: String,

    #[arg(
        long,
        env = "PROFILE_IMAGES_RENDERING_DRIVER",
        default_value = "opengl3"
    )]
    pub profile_images_rendering_driver: String,

    #[arg(long, env = "PROFILE_IMAGES_DCLENV", default_value = "")]
    pub profile_images_dclenv: String,

    /// Refused together with the renderer (the avatar scene needs a display).
    #[arg(long, env = "PROFILE_IMAGES_GODOT_HEADLESS", default_value_t = false, action = clap::ArgAction::Set)]
    pub profile_images_godot_headless: bool,

    #[arg(long, env = "PROFILE_IMAGES_GODOT_DISPLAY", default_value = "")]
    pub profile_images_godot_display: String,

    #[arg(long, env = "PROFILE_IMAGES_GODOT_EXTRA_ARGS", default_value = "")]
    pub profile_images_godot_extra_args: String,

    #[arg(
        long,
        env = "PROFILE_IMAGES_RENDER_TIMEOUT_SECONDS",
        default_value_t = 120
    )]
    pub profile_images_render_timeout_seconds: u64,

    #[arg(
        long,
        env = "PROFILE_IMAGES_RENDER_MAX_CONCURRENT",
        default_value_t = 1
    )]
    pub profile_images_render_max_concurrent: u64,

    /// Empty = `<cache dir>/.render-tmp`.
    #[arg(long, env = "PROFILE_IMAGES_RENDER_WORKDIR", default_value = "")]
    pub profile_images_render_workdir: String,

    /// The Segment destination webhooks (`/events-notifier/forward*`): the
    /// shared key of the `x-signature` HMAC. Unset, the routes answer 503.
    #[arg(long, env = "SEGMENT_SIGNING_KEY", hide_env_values = true)]
    pub segment_signing_key: Option<String>,

    /// Refuse an explorer event whose context fails the soft checks (renderer,
    /// event type, address, auth-chain shape, realm) instead of only logging them.
    #[arg(long, env = "FORCE_EXPLORER_CONTEXT_VALIDATION", default_value_t = false, action = clap::ArgAction::Set)]
    pub force_explorer_context_validation: bool,

    /// The realm name an explorer event must report, with the URL below;
    /// unset, the realm check is skipped (the notifier outside production).
    #[arg(long, env = "SEGMENT_EXPECTED_REALM")]
    pub segment_expected_realm: Option<String>,

    #[arg(long, env = "SEGMENT_EXPECTED_REALM_URL")]
    pub segment_expected_realm_url: Option<String>,

    /// The Segment project whose `identify` events are the web auth-identify events.
    #[arg(
        long,
        env = "SEGMENT_WEB_PROJECT_ID",
        default_value = "ncCetmghaPEGFVVa1n8hKY"
    )]
    pub segment_web_project_id: String,

    /// Days a `social.client_events` row is kept before the purge deletes it.
    #[arg(long, env = "CLIENT_EVENTS_RETENTION_DAYS", default_value_t = 30)]
    pub client_events_retention_days: u32,
}

/// The Pulse room authority (`comms::gatekeeper`).
#[derive(Args, Clone, Debug)]
pub struct RoomAuthorityConfig {
    // comms::gatekeeper: the Pulse room authority (the application relay's
    // admission check, POST /internal/pulse/room-authority/v1).
    /// 32 base64url bytes without padding (or _FILE), distinct from the
    /// LiveKit secret; unset, the room authority route is not mounted.
    #[arg(long, env = "PULSE_ROOM_AUTHORITY_KEY", hide_env_values = true)]
    pub pulse_room_authority_key: Option<String>,
}

/// A finite number above zero (upstream refuses to start otherwise).
fn positive(raw: &str) -> Result<f64, String> {
    match raw.parse::<f64>() {
        Ok(value) if value.is_finite() && value > 0.0 => Ok(value),
        _ => Err(format!("`{raw}` is not a positive number")),
    }
}

impl SocialConfig {
    /// Each source provider is configured by its own settings and then needs
    /// the strings the source requires for it: analytics its context, URL and
    /// token; AI compliance its OpenAI key; the referral Slack effect `ENV`
    /// (source `referral.ts` `requireString('ENV')`). `ENV` alone configures
    /// nothing, so a node can set it without every provider's credentials,
    /// and the posture's empty analytics URL alone is an inactive default.
    pub fn validate_provider_bootstrap(&self) -> Result<(), String> {
        let analytics = self.social_analytics_context.is_some()
            || self.social_analytics_api_token.is_some()
            || self.social_analytics_request_timeout_ms.is_some()
            || self
                .social_analytics_api_url
                .as_deref()
                .is_some_and(|value| !value.is_empty());
        let compliance = self.community_compliance_api_key.is_some()
            || self.community_compliance_organization.is_some()
            || self.community_compliance_project.is_some()
            || (!self.community_compliance_model.is_empty()
                && self.community_compliance_model != "gpt-5-nano")
            || (!self.community_compliance_base_url.is_empty()
                && self.community_compliance_base_url != "https://api.openai.com/v1");
        let slack = self.referral_slack_token.is_some();
        // Source components initialize analytics, AI compliance, then the
        // referral component. requireString distinguishes an explicitly empty
        // string from absence.
        let required = [
            (
                analytics,
                &self.social_analytics_context,
                "ANALYTICS_CONTEXT",
            ),
            (
                analytics,
                &self.social_analytics_api_url,
                "ANALYTICS_API_URL",
            ),
            (
                analytics,
                &self.social_analytics_api_token,
                "ANALYTICS_API_TOKEN",
            ),
            (analytics, &self.social_environment, "ENV"),
            (
                compliance,
                &self.community_compliance_api_key,
                "OPEN_AI_API_KEY",
            ),
            (slack, &self.social_environment, "ENV"),
        ];
        for (active, value, name) in required {
            if active && value.is_none() {
                return Err(format!("Configuration: string {name} is required"));
            }
        }
        Ok(())
    }
}

impl SocialConfig {
    /// SENDGRID_FROM_EMAIL, else upstream's SENDGRID_EMAIL_FROM; empty counts as unset.
    pub fn sendgrid_sender(&self) -> Option<&str> {
        first_set(&self.sendgrid_from_email, &self.sendgrid_email_from)
    }

    /// SENDGRID_NOTIFICATION_TEMPLATE_ID, else upstream's SENDGRID_EMAIL_TEMPLATE_ID; empty counts as unset.
    pub fn sendgrid_template_id(&self) -> Option<&str> {
        first_set(
            &self.sendgrid_notification_template_id,
            &self.sendgrid_email_template_id,
        )
    }
}

fn first_set<'a>(primary: &'a Option<String>, alias: &'a Option<String>) -> Option<&'a str> {
    [primary, alias]
        .into_iter()
        .filter_map(|v| v.as_deref())
        .find(|v| !v.is_empty())
}

impl std::fmt::Debug for SocialConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SocialConfig").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use crate::config::Config;

    const BASE: [(&str, &str); 1] = [("DATABASE_URL", "postgres:///catalyst")];

    #[test]
    fn http_body_cap_uses_executed_source_boot_rules() {
        let source:serde_json::Value=serde_json::from_str(include_str!(concat!(env!("DCL_ONE_ROOT"),"/services/catalyst/tests/fixtures/conformance/social/source-contracts/community-body-limit.json"))).unwrap();
        let default = social(&[]).unwrap().social_http_max_request_body_bytes;
        for case in source["boot"].as_array().unwrap() {
            let result = case["input"]
                .as_str()
                .map(super::source_body_limit)
                .unwrap_or(Ok(default));
            if let Some(expected) = case["result"].as_u64() {
                assert_eq!(result.unwrap(), expected as usize, "{}", case["input"]);
            } else {
                assert_eq!(
                    result.unwrap_err(),
                    case["error"].as_str().unwrap(),
                    "{}",
                    case["input"]
                );
            }
        }
        assert_eq!(
            social(&[("HTTP_MAX_REQUEST_BODY_BYTES", "64junk")])
                .unwrap()
                .social_http_max_request_body_bytes,
            64
        );
        for invalid in ["0", "-1", "1.5", "16777217", "NaN", "Infinity", ""] {
            assert!(
                social(&[("HTTP_MAX_REQUEST_BODY_BYTES", invalid)]).is_err(),
                "{invalid}"
            );
        }
    }

    fn social(pairs: &[(&str, &str)]) -> Result<super::SocialConfig, clap::Error> {
        Config::parse_env_for_tests(BASE.iter().chain(pairs).copied()).map(|c| c.social)
    }

    #[test]
    fn presence_badge_settings_are_bounded() {
        let defaults = social(&[]).unwrap();
        assert_eq!(
            defaults.badges_presence_early_join_lookahead_seconds,
            super::presence_badges::EARLY_JOIN_LOOKAHEAD_SECONDS
        );
        assert_eq!(
            social(&[("BADGES_PRESENCE_EARLY_JOIN_LOOKAHEAD_SECONDS", "0")])
                .unwrap()
                .badges_presence_early_join_lookahead_seconds,
            0
        );
        for (name, invalid) in [
            ("BADGES_PRESENCE_EARLY_JOIN_LOOKAHEAD_SECONDS", "2592001"),
            (
                "BADGES_PRESENCE_EARLY_JOIN_LOOKAHEAD_SECONDS",
                "18446744073709551615",
            ),
            ("BADGES_PRESENCE_OPEN_VISIT_MAX_AGE_SECONDS", "0"),
            ("BADGES_PRESENCE_SWEEP_INTERVAL_SECONDS", "2592001"),
            ("BADGES_PRESENCE_PRUNE_BATCH_SIZE", "100001"),
        ] {
            assert!(social(&[(name, invalid)]).is_err(), "{name}={invalid}");
        }
    }

    #[test]
    fn sendgrid_accepts_upstreams_variable_names_as_fallbacks() {
        let defaults = social(&[]).unwrap();
        assert_eq!(defaults.sendgrid_sender(), None);
        assert_eq!(defaults.sendgrid_template_id(), None);
        assert_eq!(defaults.sendgrid_api_url, "https://api.sendgrid.com");
        let upstream = social(&[
            ("SENDGRID_EMAIL_FROM", "from@example.com"),
            ("SENDGRID_EMAIL_TEMPLATE_ID", "d-upstream"),
            ("SENDGRID_API_URL", "https://sendgrid.example"),
        ])
        .unwrap();
        assert_eq!(upstream.sendgrid_sender(), Some("from@example.com"));
        assert_eq!(upstream.sendgrid_template_id(), Some("d-upstream"));
        assert_eq!(upstream.sendgrid_api_url, "https://sendgrid.example");
        let both = social(&[
            ("SENDGRID_FROM_EMAIL", "own@example.com"),
            ("SENDGRID_EMAIL_FROM", "from@example.com"),
            ("SENDGRID_NOTIFICATION_TEMPLATE_ID", ""),
            ("SENDGRID_EMAIL_TEMPLATE_ID", "d-upstream"),
        ])
        .unwrap();
        assert_eq!(both.sendgrid_sender(), Some("own@example.com"));
        assert_eq!(both.sendgrid_template_id(), Some("d-upstream"));
    }

    #[test]
    fn broadcasts_are_kept_forever_by_default() {
        assert_eq!(social(&[]).unwrap().notifications_broadcast_horizon_days, 0);
    }

    #[test]
    fn segment_webhook_settings_default_to_the_notifier_outside_production() {
        let defaults = social(&[]).unwrap();
        assert_eq!(defaults.segment_signing_key, None);
        assert!(!defaults.force_explorer_context_validation);
        assert_eq!(defaults.segment_expected_realm, None);
        assert_eq!(defaults.segment_expected_realm_url, None);
        assert_eq!(defaults.segment_web_project_id, "ncCetmghaPEGFVVa1n8hKY");
        assert_eq!(defaults.client_events_retention_days, 30);
        let configured = social(&[
            ("SEGMENT_SIGNING_KEY", "k"),
            ("FORCE_EXPLORER_CONTEXT_VALIDATION", "true"),
            ("SEGMENT_EXPECTED_REALM", "main"),
            ("CLIENT_EVENTS_RETENTION_DAYS", "7"),
        ])
        .unwrap();
        assert_eq!(configured.segment_signing_key.as_deref(), Some("k"));
        assert!(configured.force_explorer_context_validation);
        assert_eq!(configured.segment_expected_realm.as_deref(), Some("main"));
        assert_eq!(configured.client_events_retention_days, 7);
    }

    #[test]
    fn active_provider_bootstrap_matches_the_source_required_strings_and_order() {
        let source: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("DCL_ONE_ROOT"), "/services/catalyst/tests/fixtures/conformance/social/provider-boundary-source-cases.json"
        ))).unwrap();
        let required = [
            "ANALYTICS_CONTEXT",
            "ANALYTICS_API_URL",
            "ANALYTICS_API_TOKEN",
            "ENV",
            "OPEN_AI_API_KEY",
            "SLACK_BOT_TOKEN",
        ];
        for case in source["active_bootstrap"].as_array().unwrap() {
            let omitted = case["omitted"].as_str();
            if omitted == Some("all-missing") {
                assert!(
                    social(&[]).unwrap().validate_provider_bootstrap().is_ok(),
                    "unconfigured monolith has no source-provider startup requirement"
                );
                continue;
            }
            let mut pairs = required
                .iter()
                .filter(|key| Some(**key) != omitted)
                .map(|key| {
                    (
                        *key,
                        if omitted == Some("all-empty") {
                            ""
                        } else {
                            "fixture"
                        },
                    )
                })
                .collect::<Vec<_>>();
            // Compliance is configured by any of its settings; without its key
            // a model configures it, and the source's key requirement applies.
            if omitted == Some("OPEN_AI_API_KEY") {
                pairs.push(("OPEN_AI_MODEL", "fixture"));
            }
            let result = social(&pairs).unwrap().validate_provider_bootstrap();
            if omitted == Some("SLACK_BOT_TOKEN") {
                // The token is the referral Slack effect's only setting: without
                // it the node runs without that effect, where the source
                // refuses to start. A deliberate difference.
                result.unwrap();
                continue;
            }
            if case["configured"] == true {
                result.unwrap();
            } else {
                assert_eq!(
                    result.unwrap_err(),
                    case["error"].as_str().unwrap(),
                    "{}",
                    case["omitted"]
                );
            }
        }
    }

    /// Each provider is gated on its own settings: `ENV` alone requires no
    /// credential, and one provider's settings require only its own strings.
    #[test]
    fn each_provider_requires_only_its_own_strings() {
        let check = |pairs: &[(&str, &str)]| social(pairs).unwrap().validate_provider_bootstrap();
        for env in ["", "prd", "production"] {
            check(&[("ENV", env)]).unwrap();
        }
        check(&[("OPEN_AI_API_KEY", "")]).unwrap();
        assert_eq!(
            check(&[("ENV", "prd"), ("OPEN_AI_MODEL", "gpt-5")]).unwrap_err(),
            "Configuration: string OPEN_AI_API_KEY is required"
        );
        assert_eq!(
            check(&[("SLACK_BOT_TOKEN", "")]).unwrap_err(),
            "Configuration: string ENV is required"
        );
        check(&[("SLACK_BOT_TOKEN", ""), ("ENV", "prd")]).unwrap();
        assert_eq!(
            check(&[("ENV", "prd"), ("ANALYTICS_API_TOKEN", "fixture")]).unwrap_err(),
            "Configuration: string ANALYTICS_CONTEXT is required"
        );
        let analytics = [
            ("ANALYTICS_CONTEXT", "fixture"),
            ("ANALYTICS_API_URL", "https://analytics.example.org"),
            ("ANALYTICS_API_TOKEN", "fixture"),
        ];
        assert_eq!(
            check(&analytics).unwrap_err(),
            "Configuration: string ENV is required"
        );
        check(&[analytics.as_slice(), &[("ENV", "prd")]].concat()).unwrap();
    }

    /// The island posture refuses a Slack token, so `ENV` must not require one:
    /// an island node with `ENV` set boots.
    #[test]
    fn island_posture_with_env_boots_without_third_party_providers() {
        for posture in [
            crate::posture::Posture::Island,
            crate::posture::Posture::Journey,
        ] {
            let expanded = crate::posture::expand(posture, &|_| None);
            let pairs = expanded
                .iter()
                .map(|(key, value)| (*key, value.as_str()))
                .chain([("ENV", "prd")])
                .collect::<Vec<_>>();
            social(&pairs)
                .unwrap()
                .validate_provider_bootstrap()
                .unwrap();
        }
    }

    #[test]
    fn posture_empty_analytics_url_keeps_unconfigured_monolith_providers_inactive() {
        for posture in [
            crate::posture::Posture::Mirror,
            crate::posture::Posture::Island,
            crate::posture::Posture::Journey,
        ] {
            let expanded = crate::posture::expand(posture, &|_| None);
            let pairs = expanded
                .iter()
                .map(|(key, value)| (*key, value.as_str()))
                .collect::<Vec<_>>();
            let settings = social(&pairs).unwrap();
            assert_eq!(settings.social_analytics_api_url.as_deref(), Some(""));
            settings.validate_provider_bootstrap().unwrap();
        }
    }

    #[test]
    fn analytics_config_preserves_source_strings_numbers_and_secret_slot() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("DCL_ONE_ROOT"),
            "/library/dcl-http/tests/fixtures/social-analytics-source-cases.json"
        )))
        .unwrap();
        let absent = social(&[]).unwrap();
        assert!(absent.social_analytics_api_token.is_none() && absent.social_environment.is_none());
        let configured = social(&[
            ("ANALYTICS_CONTEXT", ""),
            ("ANALYTICS_API_URL", ""),
            ("ANALYTICS_API_TOKEN", "fixture-token"),
            ("ENV", ""),
        ])
        .unwrap();
        assert_eq!(configured.social_analytics_context.as_deref(), Some(""));
        assert_eq!(configured.social_environment.as_deref(), Some(""));
        assert!(!format!("{configured:?}").contains("fixture-token"));
        for case in fixture["timeouts"].as_array().unwrap() {
            let raw = case["raw"].as_str();
            let result = match raw {
                Some(raw) => social(&[("ANALYTICS_REQUEST_TIMEOUT", raw)]),
                None => social(&[]),
            };
            if !case["error"].is_null() {
                assert!(result.is_err());
                continue;
            }
            let parsed = result.unwrap().social_analytics_request_timeout_ms;
            match &case["configured_number"] {
                serde_json::Value::Null => assert!(parsed.is_none()),
                serde_json::Value::String(value) => {
                    assert!(parsed.unwrap().is_infinite());
                    assert_eq!(parsed.unwrap().is_sign_negative(), value.starts_with('-'));
                }
                expected => assert_eq!(parsed.unwrap(), expected.as_f64().unwrap()),
            }
        }
        let (mut config, matches) = Config::parse_with_matches_for_tests(&[
            "catalyst",
            "--database-url",
            "postgres://x/y",
            "--analytics-api-token",
            "argv-token",
        ])
        .unwrap();
        let slots = config.secret_slots();
        assert_eq!(
            slots.len(),
            slots
                .iter()
                .map(|(key, _)| *key)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            "secret slot names must be unique"
        );
        assert_eq!(
            slots
                .iter()
                .find(|(key, _)| *key == "ANALYTICS_API_TOKEN")
                .map(|(_, value)| value.as_deref()),
            Some(Some("argv-token"))
        );
        assert!(
            config
                .refuse_argv_secrets(&matches)
                .unwrap_err()
                .to_string()
                .contains("ANALYTICS_API_TOKEN")
        );
    }

    #[test]
    fn legacy_uws_status_and_metrics_keep_raw_source_strings() {
        let config = social(&[
            ("CURRENT_VERSION", ""),
            ("WKC_METRICS_PUBLIC_PATH", ""),
            ("WKC_METRICS_BEARER_TOKEN", "  "),
            ("WKC_METRICS_RESET_AT_NIGHT", "TRUE"),
        ])
        .unwrap();
        assert_eq!(config.social_rpc_current_version.as_deref(), Some(""));
        assert_eq!(config.wkc_metrics_public_path.as_deref(), Some(""));
        assert_eq!(config.wkc_metrics_bearer_token.as_deref(), Some("  "));
        assert_eq!(config.wkc_metrics_reset_at_night.as_deref(), Some("TRUE"));
        let config = social(&[
            ("CURRENT_VERSION", "source-version"),
            ("WKC_METRICS_PUBLIC_PATH", "/custom"),
            ("WKC_METRICS_RESET_AT_NIGHT", "true"),
        ])
        .unwrap();
        assert_eq!(
            config.social_rpc_current_version.as_deref(),
            Some("source-version")
        );
        assert_eq!(config.wkc_metrics_public_path.as_deref(), Some("/custom"));
        assert_eq!(config.wkc_metrics_reset_at_night.as_deref(), Some("true"));
    }

    #[test]
    fn friendship_limits_read_source_variables_including_zero_and_negative_values() {
        let defaults = social(&[]).unwrap();
        assert_eq!(
            (
                defaults.friendship_rate_limit_window_seconds,
                defaults.friendship_rate_limit_per_actor,
                defaults.friendship_rate_limit_per_pair
            ),
            (60.0, 30.0, 10.0)
        );
        let configured = social(&[
            ("FRIENDSHIP_RATE_LIMIT_WINDOW_SECONDS", "-1"),
            ("FRIENDSHIP_RATE_LIMIT_PER_ACTOR", "0"),
            ("FRIENDSHIP_RATE_LIMIT_PER_PAIR", "2.5"),
        ])
        .unwrap();
        assert_eq!(
            (
                configured.friendship_rate_limit_window_seconds,
                configured.friendship_rate_limit_per_actor,
                configured.friendship_rate_limit_per_pair
            ),
            (-1.0, 0.0, 2.5)
        );
    }

    // Executed source values: source-contracts/referral-numbers.json.
    #[test]
    fn referral_numbers_use_the_executed_source_configuration_values() {
        for name in ["REFERRAL_MAX_IP_MATCHES", "REFERRAL_MIN_LOGIN_DAYS"] {
            for (raw, expected) in [
                ("2tail", 2.0),
                ("2.5days", 2.5),
                ("0", 0.0),
                ("-1", -1.0),
                ("Infinity", f64::INFINITY),
                ("-Infinity", f64::NEG_INFINITY),
                ("1e20", 1e20),
                ("1e21", 1e21),
                ("1e-7", 1e-7),
            ] {
                let parsed = social(&[(name, raw)]).unwrap();
                let value = if name == "REFERRAL_MAX_IP_MATCHES" {
                    parsed.referral_max_ip_matches
                } else {
                    parsed.referral_min_login_days
                };
                assert_eq!(value, expected, "{name}={raw}");
            }
            assert!(social(&[(name, "NaN")]).is_err());
        }
    }

    #[test]
    fn referral_slack_uses_source_configuration_and_numeric_window() {
        let defaults = social(&[]).unwrap();
        assert!(defaults.referral_slack_token.is_none());
        assert_eq!(defaults.referral_suspicious_window_ms, 300_000.0);
        let configured = social(&[
            ("SLACK_BOT_TOKEN", "fixture-token"),
            (
                "REFERRAL_METABASE_DASHBOARD",
                "https://dashboard.example.org",
            ),
            ("REFERRAL_FIVE_MINUTES_IN_MS", " 1.5e3ms"),
            ("ENV", "dev"),
        ])
        .unwrap();
        assert_eq!(
            configured.referral_slack_token.as_deref(),
            Some("fixture-token")
        );
        assert_eq!(
            configured.referral_metabase_dashboard,
            "https://dashboard.example.org"
        );
        assert_eq!(configured.referral_suspicious_window_ms, 1500.0);
        assert_eq!(configured.social_environment.as_deref(), Some("dev"));
        assert!(social(&[("REFERRAL_FIVE_MINUTES_IN_MS", "NaN")]).is_err());
    }

    #[test]
    fn friendship_numbers_use_source_parse_float_instead_of_strict_rust_parsing() {
        for (raw, expected) in [
            ("10abc", 10.0),
            ("  +.5tail", 0.5),
            ("1e+2suffix", 100.0),
            ("1e-", 1.0),
            ("0x10", 0.0),
            ("Infinitysuffix", f64::INFINITY),
            ("-Infinity", f64::NEG_INFINITY),
            ("1e999", f64::INFINITY),
        ] {
            let configured = social(&[("FRIENDSHIP_RATE_LIMIT_PER_PAIR", raw)]).unwrap();
            assert_eq!(configured.friendship_rate_limit_per_pair, expected, "{raw}");
        }
        for raw in ["NaN", "invalid", "", "infinity"] {
            assert!(
                social(&[("FRIENDSHIP_RATE_LIMIT_PER_PAIR", raw)]).is_err(),
                "{raw}"
            );
        }
    }

    #[test]
    fn ordinary_transport_settings_match_source_prefix_numbers_and_defaults() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("DCL_ONE_ROOT"),
            "/services/catalyst/tests/fixtures/conformance/social/ws-transport-source-cases.json"
        )))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let values: Vec<_> = case["config"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str().unwrap()))
                .collect();
            let configured = social(&values).unwrap();
            let number = |name: &str| {
                case["configuration"][name].as_f64().unwrap_or_else(|| {
                    case["configuration"][name]
                        .as_str()
                        .unwrap()
                        .parse()
                        .unwrap()
                })
            };
            assert_eq!(
                configured.ws_transport_max_queue_size,
                number("max_queue_size"),
                "{}",
                case["id"]
            );
            assert_eq!(
                configured.ws_transport_min_queue_size,
                number("min_queue_size"),
                "{}",
                case["id"]
            );
            assert_eq!(
                configured.ws_transport_max_queue_size_limit,
                number("max_queue_size_limit"),
                "{}",
                case["id"]
            );
            assert_eq!(
                configured.ws_transport_estimated_message_size,
                number("estimated_message_size"),
                "{}",
                case["id"]
            );
            assert_eq!(
                configured.ws_transport_retry_delay_ms,
                number("retry_delay_ms"),
                "{}",
                case["id"]
            );
            assert_eq!(
                configured.ws_transport_max_retry_attempts,
                number("max_retry_attempts"),
                "{}",
                case["id"]
            );
            assert_eq!(
                configured.ws_transport_max_backoff_delay_ms,
                number("max_backoff_delay_ms"),
                "{}",
                case["id"]
            );
        }
        assert!(social(&[("WS_TRANSPORT_MAX_QUEUE_SIZE", "NaN")]).is_err());
    }

    #[test]
    fn backpressure_numbers_preserve_source_raw_value_before_native_conversion() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("DCL_ONE_ROOT"),
            "/services/catalyst/tests/fixtures/conformance/social/ws-backpressure-source-cases.json"
        )))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let raw = case["raw"].as_str();
            let configured = social(
                &raw.map(|raw| vec![("WS_MAX_BACKPRESSURE", raw)])
                    .unwrap_or_default(),
            );
            if case.get("error").is_some() {
                assert!(configured.is_err(), "{raw:?}");
                continue;
            }
            let expected = case["number"]
                .as_f64()
                .unwrap_or_else(|| case["number"].as_str().unwrap().parse().unwrap());
            assert_eq!(configured.unwrap().ws_max_backpressure, expected, "{raw:?}");
        }
    }

    #[test]
    fn socket_numeric_settings_match_executed_source_and_native_idle_admission() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("DCL_ONE_ROOT"),
            "/services/catalyst/tests/fixtures/conformance/social/ws-numbers-source-cases.json"
        )))
        .unwrap();
        for case in fixture["auth"].as_array().unwrap() {
            for field in [
                "WS_AUTH_TIMEOUT_IN_MS",
                "WS_AUTH_SIGNATURE_EXPIRATION_IN_MS",
            ] {
                let raw = case["raw"].as_str();
                let values = raw.map(|raw| vec![(field, raw)]).unwrap_or_default();
                let actual = social(&values);
                if case.get("error").is_some() {
                    assert!(actual.is_err(), "{field} {raw:?}");
                    continue;
                }
                let configured = actual.unwrap();
                let number = if field == "WS_AUTH_TIMEOUT_IN_MS" {
                    configured.ws_auth_timeout_in_ms
                } else {
                    configured.ws_auth_signature_expiration_in_ms
                }
                .unwrap_or(300000.0);
                let expected = case["number"]
                    .as_f64()
                    .unwrap_or_else(|| case["number"].as_str().unwrap().parse().unwrap());
                assert_eq!(number, expected, "{field} {raw:?}");
            }
        }
        for case in fixture["idle"].as_array().unwrap() {
            let raw = case["raw"].as_str().unwrap();
            let actual = social(&[("WS_IDLE_TIMEOUT_IN_SECONDS", raw)]);
            if case["native_allowed"] == true {
                assert_eq!(
                    actual.unwrap().ws_idle_timeout_in_seconds,
                    case["native_seconds"].as_u64().unwrap(),
                    "{raw}"
                );
            } else {
                assert!(actual.is_err(), "{raw}");
            }
        }
    }

    #[test]
    fn socket_deadline_signature_age_and_idle_have_independent_source_variables() {
        let defaults = social(&[]).unwrap();
        assert_eq!(defaults.ws_idle_timeout_in_seconds, 300);
        assert_eq!(defaults.ws_auth_timeout_in_ms, None);
        let config = social(&[
            ("WS_AUTH_TIMEOUT_IN_MS", "7"),
            ("WS_AUTH_SIGNATURE_EXPIRATION_IN_MS", "23"),
            ("WS_IDLE_TIMEOUT_IN_SECONDS", "0"),
        ])
        .unwrap();
        assert_eq!(config.ws_auth_timeout_in_ms, Some(7.0));
        assert_eq!(config.ws_auth_signature_expiration_in_ms, Some(23.0));
        assert_eq!(config.ws_idle_timeout_in_seconds, 0);
    }

    /// The social sockets' limits are upstream social-service-ea's variables
    /// with its defaults (`src/logic/ws-admission/component.ts:19-22`,
    /// `stream-ws-handler.ts:28`, `stream-sessions/component.ts:68-69`,
    /// `ws-handler.ts:272`), and a value upstream refuses to start with is refused.
    #[test]
    fn socket_limits_read_upstreams_variables() {
        let defaults = social(&[]).unwrap();
        assert_eq!(defaults.ws_max_backpressure, 131072.0);
        assert_eq!(defaults.ws_max_connections_per_address, 8);
        assert_eq!(defaults.ws_subscriptions_per_second, 2.0);
        assert_eq!(defaults.ws_subscription_burst, 24);
        assert_eq!(defaults.ws_address_budget_capacity, 10_000);
        assert_eq!(defaults.stream_ws_max_payload_length, 16 * 1024);
        assert_eq!(defaults.stream_ws_messages_per_second, 20.0);
        assert_eq!(defaults.stream_ws_message_burst, 40);

        let set = social(&[
            ("WS_MAX_BACKPRESSURE", "1024"),
            ("WS_MAX_CONNECTIONS_PER_ADDRESS", "2"),
            ("WS_SUBSCRIPTIONS_PER_SECOND", "0.5"),
            ("WS_SUBSCRIPTION_BURST", "3"),
            ("WS_ADDRESS_BUDGET_CAPACITY", "4"),
            ("STREAM_WS_MAX_PAYLOAD_LENGTH", "5"),
            ("STREAM_WS_MESSAGES_PER_SECOND", "6.5"),
            ("STREAM_WS_MESSAGE_BURST", "7"),
        ])
        .unwrap();
        assert_eq!(
            (
                set.ws_max_backpressure,
                set.ws_max_connections_per_address,
                set.ws_subscriptions_per_second,
                set.ws_subscription_burst,
                set.ws_address_budget_capacity,
                set.stream_ws_max_payload_length,
                set.stream_ws_messages_per_second,
                set.stream_ws_message_burst,
            ),
            (1024.0, 2, 0.5, 3, 4, 5, 6.5, 7)
        );
        for refused in [
            ("WS_MAX_CONNECTIONS_PER_ADDRESS", "0"),
            ("WS_SUBSCRIPTIONS_PER_SECOND", "0"),
            ("WS_SUBSCRIPTION_BURST", "0"),
            ("WS_ADDRESS_BUDGET_CAPACITY", "0"),
            ("STREAM_WS_MAX_PAYLOAD_LENGTH", "0"),
            ("STREAM_WS_MESSAGES_PER_SECOND", "-1"),
            ("STREAM_WS_MESSAGES_PER_SECOND", "inf"),
            ("STREAM_WS_MESSAGE_BURST", "0"),
        ] {
            assert!(social(&[refused]).is_err(), "{refused:?}");
        }
    }
}
