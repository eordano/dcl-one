//! The process-level settings and the local-authority write switches.

use clap::Args;

/// The process: its database pool, HTTP listener and public URLs, and which jobs it runs.
#[derive(Args, Clone, Debug)]
pub struct NodeConfig {
    /// mirror, island or journey: the defaults of every sync, mirror, index,
    /// upstream, cache and write variable this one name expands into before
    /// the arguments are parsed (`catalyst posture show <name>`; a variable
    /// set in the environment wins). Unset: every upstream and write is off.
    /// Read from the environment alone.
    #[arg(long, env = "NODE_POSTURE")]
    pub node_posture: Option<String>,

    /// Postgres DSN of the `catalyst` database, e.g.
    /// postgres:///catalyst?host=/run/postgresql&user=catalyst (peer auth: no
    /// password; one that carries one is still never printed by --help)
    #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
    pub database_url: String,

    /// The ONE pool: the old content pool (20) plus the old sync pool (40)
    /// served the same database from one process, so 60 is their sum.
    #[arg(long, env = "PG_POOL_SIZE", default_value_t = 60)]
    pub pg_pool_size: u32,

    /// The sync worker's own pool, apart from PG_POOL_SIZE so a bootstrap can never take the
    /// request handlers' connections. 0 derives a third of PG_POOL_SIZE (2..=16); any value is
    /// capped below PG_POOL_SIZE.
    #[arg(long, env = "SYNC_PG_POOL_SIZE", default_value_t = 0)]
    pub sync_pg_pool_size: u32,

    #[arg(long, env = "HTTP_SERVER_HOST", default_value = "127.0.0.1")]
    pub http_server_host: String,

    /// Registered in ports.toml as the catalyst-v2 dev HTTP port.
    #[arg(long, env = "HTTP_SERVER_PORT", default_value_t = 5210)]
    pub http_server_port: u16,

    /// Direct backend proxies allowed to supply a single, overwritten X-Real-IP
    /// for referral quotas and the `/contents` rate limit. Empty trusts only the
    /// socket peer. These are the backend proxy networks, not the proxy's own
    /// upstream CDN networks.
    #[arg(long, env = "HTTP_TRUSTED_PROXY_CIDRS", value_delimiter = ',')]
    pub http_trusted_proxy_cidrs: Vec<ipnet::IpNet>,

    /// Third-party hosts the scene proxy forwards to (`scene_proxy` in the root crate).
    #[arg(long, env = "SCENE_PROXY_HOSTS", value_delimiter = ',')]
    pub scene_proxy_hosts: Vec<String>,

    /// Operator-local Prometheus scrape listener, separate from the application API.
    #[arg(long, env = "SERVE_METRICS_BIND", default_value = "127.0.0.1:5006")]
    pub serve_metrics_bind: String,

    #[arg(
        long,
        env = "STORAGE_ROOT_FOLDER",
        default_value = "/var/lib/catalyst/content"
    )]
    pub storage_root_folder: String,

    #[arg(long, env = "RUST_LOG", default_value = "info")]
    pub rust_log: String,

    /// Base URL clients reach this node at; content/lambdas URLs derive from
    /// it unless given. Defaults to http://<bind address>.
    #[arg(long, env = "PUBLIC_URL")]
    pub public_url: Option<String>,

    #[arg(long, env = "CONTENT_URL")]
    pub content_url: Option<String>,

    #[arg(long, env = "LAMBDAS_URL")]
    pub lambdas_url: Option<String>,

    #[arg(long, env = "REALM_NAME")]
    pub realm_name: Option<String>,

    #[arg(long, env = "ETH_NETWORK", default_value = "mainnet")]
    pub eth_network: String,

    #[arg(long, env = "ENTITIES_CACHE_CONTROL_MAX_AGE", default_value_t = 10)]
    pub entities_cache_control_max_age: u64,

    /// nginx `X-Accel-Redirect` prefix for GET /contents (production sets
    /// /__protected_storage); unset streams the blob from this process.
    #[arg(long, env = "STORAGE_X_ACCEL_BASE")]
    pub storage_x_accel_base: Option<String>,

    /// The satellite tiles /about advertises (`<base>/{z}/{x}/{y}.jpg`); empty
    /// is this node's own `<PUBLIC_URL>/satellite`.
    #[arg(long, env = "MAP_SATELLITE_BASE_URL", default_value = "")]
    pub map_satellite_base_url: String,

    /// The bearer of `/onboarding/{checkpoint,pending-nudges}` (or
    /// ONBOARDING_API_KEY_FILE); unset, both answer 401.
    #[arg(long, env = "ONBOARDING_API_KEY", hide_env_values = true)]
    pub onboarding_api_key: Option<String>,

    /// The map service `/lambdas/users/{address}/lands` points its parcel and
    /// estate images at (`<base>/v1/parcels/{x}/{y}/map.png`); empty is this
    /// node's own `<PUBLIC_URL>/api`. `NODE_POSTURE=mirror` sets
    /// `https://api.decentraland.org`, the base of the subgraph's images.
    #[arg(long, env = "LAND_IMAGE_BASE_URL", default_value = "")]
    pub land_image_base_url: String,

    /// The minimap image /about advertises; empty is this node's own
    /// `<PUBLIC_URL>/api/v1/minimap.png`.
    #[arg(long, env = "MAP_PARCEL_VIEW_URL", default_value = "")]
    pub map_parcel_view_url: String,

    #[arg(long, env = "DEPLOYMENTS_QUERY_TIMEOUT_SECS", default_value_t = 10)]
    pub deployments_query_timeout_secs: u64,

    /// Whole-request timeout; 0 disables it.
    #[arg(long, env = "REQUEST_TIMEOUT_SECS", default_value_t = 60)]
    pub request_timeout_secs: u64,

    /// Overrides the build-time CATALYST_COMMIT_HASH in /about and /status.
    #[arg(long, env = "COMMIT_HASH")]
    pub commit_hash: Option<String>,

    /// lamb2's overload check: /about answers `acceptingUsers: false` while the
    /// host's one-minute load average per CPU or its used memory is above
    /// 90%. Off by default under every posture: the figures are host-wide,
    /// and a node shares its host with sync, Postgres and builds.
    #[arg(long, env = "ABOUT_RESOURCE_CHECK", default_value_t = false, action = clap::ArgAction::Set)]
    pub about_resource_check: bool,

    /// Development only. Report `Syncing` (healthy, accepting users) although no
    /// sync worker is attached, so the content read contracts can be replayed
    /// against whatever the database holds. Without it a node without a worker
    /// stays `Bootstrapping` and answers /about with 503. Deploys keep the
    /// bootstrapping 503 while SYNC_SOURCE names a peer; an empty SYNC_SOURCE
    /// (no sync at all) makes the node its own authority.
    #[arg(long, env = "SERVE_UNSYNCED", default_value_t = false)]
    pub serve_unsynced: bool,

    /// Development only, with SERVE_UNSYNCED while SYNC_SOURCE names a peer:
    /// the entity types (comma-separated, e.g. `profile`) this unsynced copy
    /// still takes signed deploys of. Each passes the full validation, and its
    /// timestamp is recent, so it supersedes the peer's older entity.
    #[arg(long, env = "SERVE_UNSYNCED_DEPLOY_TYPES", default_value = "")]
    pub serve_unsynced_deploy_types: String,

    /// Attach the peer sync worker: bootstrap from `SYNC_SOURCE`'s snapshots
    /// and pointer changes, then follow it. The worker owns the
    /// Bootstrapping -> Syncing transition /about reports.
    #[arg(long, env = "SYNC_ENABLED", default_value_t = false)]
    pub sync_enabled: bool,

    /// Run the fleet-wide jobs in this process: sync, the upstream mirrors,
    /// the chain indexers, the prunes, everything that changes the shared
    /// database, the storage tree or an upstream. Every API replica must set
    /// it false: a process with it true also serves comms' /ws, /ws/v4 and the
    /// socket-reading connector routes and runs the island subscriber, which
    /// mints every cluster change. False, the process runs only the
    /// per-process jobs (projection reloads, cache sweeps, its own buffers)
    /// and mounts none of those routes.
    #[arg(long, env = "FLEET_JOBS_ENABLED", default_value_t = true, action = clap::ArgAction::Set)]
    pub fleet_jobs_enabled: bool,

    /// Hold a Postgres session advisory lock per fleet-wide job, so several
    /// processes on one database run each of them in one process at a time
    /// (`jobs::Leases`). Off, they run unconditionally: a node that never
    /// runs a second process.
    #[arg(long, env = "JOB_LEASES_ENABLED", default_value_t = true, action = clap::ArgAction::Set)]
    pub job_leases_enabled: bool,

    /// Prefix of the lease lock names. Two fleets sharing one Postgres
    /// cluster must not share it.
    #[arg(long, env = "JOB_LEASE_NAMESPACE", default_value = "catalyst")]
    pub job_lease_namespace: String,

    /// Run every periodic job's first pass at startup, without jitter or a
    /// first interval. For tests that must see each job's first pass (and
    /// what it sends) within seconds; a fleet keeps the spread.
    #[arg(long, env = "JOBS_FIRST_PASS_NOW", default_value_t = false, action = clap::ArgAction::Set)]
    pub jobs_first_pass_now: bool,
}

impl NodeConfig {
    pub fn sync_pool_size(&self) -> u32 {
        let cap = self.pg_pool_size.saturating_sub(1).max(1);
        match self.sync_pg_pool_size {
            0 => (self.pg_pool_size / 3).clamp(2, 16).min(cap),
            n => n.min(cap),
        }
    }

    pub fn bind_addr(&self) -> String {
        format!("{}:{}", self.http_server_host, self.http_server_port)
    }

    /// `PUBLIC_URL` without its trailing slash, else `http://<bind address>`.
    pub fn own_url(&self) -> String {
        self.public_url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .map(|u| u.trim_end_matches('/').to_string())
            .unwrap_or_else(|| format!("http://{}", self.bind_addr()))
    }

    pub fn init_tracing(&self) {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(&self.rust_log))
            .with_target(false)
            .init();
    }
}

/// The local-authority switches of the social and content planes and of the data plane's
/// market half, and the upstream local content deploys are forwarded to (with both forwarding
/// lanes' retry schedule); the creator, worlds and operator planes keep theirs beside their
/// settings.
#[derive(Args, Clone, Debug)]
pub struct WritesConfig {
    // social: the first slice where this node is the authority for user
    // writes (communities, friendships, blocks, settings, moderation,
    // notifications, badges). Off by default: every mutating request to a
    // social route answers 503 until a backup restore of this node's
    // database, blobs and credentials has been demonstrated (DATABASE.md,
    // "Bootstrap, schema versions and local writes").
    #[arg(long, env = "SOCIAL_WRITES_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub social_writes_enabled: bool,

    // content writes (src/content/writes.rs): the content server's local
    // write path, POST /entities and DELETE /scenes/{coord}. Off by default:
    // both answer 503 until a backup restore of this node's database, blobs
    // and credentials has been demonstrated (DATABASE.md, "Bootstrap, schema versions and
    // local writes"); the social switch's twin.
    #[arg(long, env = "CONTENT_WRITES_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub content_writes_enabled: bool,

    /// A catalyst base URL every local deploy is sent on to, unchanged, at
    /// `<url>/content/entities` once it committed here (src/content/forward.rs;
    /// local authority is unchanged). Unset forwards nothing. A public http(s)
    /// origin with no path (not the `/content` URL) that is not this node,
    /// checked at startup.
    #[arg(long, env = "CONTENT_FORWARD_UPSTREAM_URL")]
    pub content_forward_upstream_url: Option<String>,

    /// Sends a forwarded deployment gets before it is `failed`: a 5xx, 404,
    /// 405, 408, 425, 429 or a transport error is retried, any other status
    /// is the upstream's verdict. Both forwarding lanes (content, worlds).
    /// The retries end sooner, unsent, once the entity is older than the
    /// upstream takes (a catalyst 20 minutes from its signature, a worlds
    /// server 5); the default schedule (about 30 minutes) outlasts both.
    #[arg(long, env = "FORWARD_MAX_ATTEMPTS", default_value_t = 12)]
    pub forward_max_attempts: u32,

    /// The first retry's delay, seconds; each retry doubles it, up to five
    /// minutes (both forwarding lanes).
    #[arg(long, env = "FORWARD_RETRY_BASE_SECS", default_value_t = 5)]
    pub forward_retry_base_secs: u64,

    /// Bearer of the content operator routes (`GET /content/forwarding`,
    /// `/content/forwarding/{entityId}` and its `POST .../retry`; or
    /// CATALYRST_CONTENT_ADMIN_TOKEN_FILE); unset refuses them.
    #[arg(long, env = "CATALYRST_CONTENT_ADMIN_TOKEN", hide_env_values = true)]
    pub catalyrst_content_admin_token: Option<String>,

    // data plane, market half: the economy relay and broker ledgers, the
    // signatures (rentals) store, the presence archive and the marketplace's
    // local lists, cancellations and moderation are local authority too, so
    // they sit behind the social switch's twin (`marketplace::DataWrites`).
    #[arg(long, env = "DATA_WRITES_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub data_writes_enabled: bool,

    /// The directory deploy/backup leaves its stamps in: `backup.json` once
    /// a snapshot's manifest is complete, `drill.json` once a restore drill
    /// passed. Set, a write switch that is on lets writes through only while
    /// both are within their budgets, checked live on each write (no
    /// restart); the 503 names the missing or stale stamp and its age.
    /// Unset, the switches alone decide.
    #[arg(long, env = "WRITES_RESTORE_STAMP_DIR")]
    pub writes_restore_stamp_dir: Option<String>,

    /// The oldest `backup.json` that still vouches for writes, hours.
    #[arg(long, env = "WRITES_BACKUP_MAX_AGE_HOURS", default_value_t = 26)]
    pub writes_backup_max_age_hours: u64,

    /// The oldest `drill.json` that still vouches for writes, days.
    #[arg(long, env = "WRITES_DRILL_MAX_AGE_DAYS", default_value_t = 8)]
    pub writes_drill_max_age_days: u64,
}

#[cfg(test)]
mod tests {
    use crate::config::Config;

    fn parse(args: &[&str]) -> Result<Config, clap::Error> {
        Config::parse_for_tests(args)
    }

    #[test]
    fn defaults_and_flags() {
        let cli = parse(&["x", "--database-url", "postgres:///catalyst"]).unwrap();
        assert_eq!(cli.node.http_server_port, 5210);
        assert_eq!(cli.node.bind_addr(), "127.0.0.1:5210");
        assert_eq!(cli.node.serve_metrics_bind, "127.0.0.1:5006");
        assert_eq!(cli.node.storage_root_folder, "/var/lib/catalyst/content");
        assert_eq!(cli.node.rust_log, "info");
        assert_eq!(cli.node.pg_pool_size, 60);
        assert_eq!(cli.node.sync_pool_size(), 16);
        for (pg, sync, want) in [(24, 0, 8), (3, 0, 2), (2, 0, 1), (24, 30, 23), (24, 4, 4)] {
            let c = super::NodeConfig {
                pg_pool_size: pg,
                sync_pg_pool_size: sync,
                ..cli.node.clone()
            };
            assert_eq!(
                c.sync_pool_size(),
                want,
                "PG_POOL_SIZE={pg} SYNC_PG_POOL_SIZE={sync}"
            );
        }
        assert_eq!(cli.node.eth_network, "mainnet");
        assert_eq!(cli.node.request_timeout_secs, 60);
        assert!(cli.node.public_url.is_none());
        assert!(
            !cli.node.serve_unsynced,
            "readiness is never advertised by default"
        );

        let cli = parse(&[
            "x",
            "--database-url",
            "postgres:///catalyst",
            "--http-server-port",
            "1",
            "--http-server-host",
            "0.0.0.0",
        ])
        .unwrap();
        assert_eq!(cli.node.bind_addr(), "0.0.0.0:1");

        let cli = parse(&[
            "x",
            "--database-url",
            "postgres:///catalyst",
            "--serve-unsynced",
        ])
        .unwrap();
        assert!(cli.node.serve_unsynced);
    }

    #[test]
    fn trusted_backend_proxy_networks_are_explicit_and_validated() {
        let defaults = parse(&["x", "--database-url", "postgres:///catalyst"]).unwrap();
        assert!(defaults.node.http_trusted_proxy_cidrs.is_empty());
        let config = parse(&[
            "x",
            "--database-url",
            "postgres:///catalyst",
            "--http-trusted-proxy-cidrs",
            "192.0.2.0/24,2001:db8::/32",
        ])
        .unwrap();
        assert_eq!(config.node.http_trusted_proxy_cidrs.len(), 2);
        assert!(
            config.node.http_trusted_proxy_cidrs[0]
                .contains(&"192.0.2.8".parse::<std::net::IpAddr>().unwrap())
        );
        assert!(
            parse(&[
                "x",
                "--database-url",
                "postgres:///catalyst",
                "--http-trusted-proxy-cidrs",
                "192.0.2.1/33"
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "x",
                "--database-url",
                "postgres:///catalyst",
                "--http-trusted-proxy-cidrs",
                "invalid"
            ])
            .is_err()
        );
    }

    #[test]
    fn the_restore_stamps_are_unset_by_default_with_a_day_and_a_week_of_budget() {
        let c = parse(&["x", "--database-url", "postgres:///catalyst"]).unwrap();
        assert_eq!(c.writes.writes_restore_stamp_dir, None);
        assert_eq!(c.writes.writes_backup_max_age_hours, 26);
        assert_eq!(c.writes.writes_drill_max_age_days, 8);
        assert_eq!(c.restore_stamp_dir(), None);
        assert!(c.write_switches_on().is_empty());
        c.require_restore_stamps()
            .expect("nothing to vouch for while every switch is off");

        let c = parse(&[
            "x",
            "--database-url",
            "postgres:///catalyst",
            "--social-writes-enabled",
            "true",
            "--ops-writes-enabled",
            "true",
            "--writes-restore-stamp-dir",
            "  ",
        ])
        .unwrap();
        assert_eq!(c.write_switches_on(), ["social", "ops"]);
        assert_eq!(c.restore_stamp_dir(), None, "a blank directory is unset");
        let err = c.require_restore_stamps().unwrap_err().to_string();
        assert!(
            err.contains("social,ops") && err.contains("WRITES_RESTORE_STAMP_DIR"),
            "{err}"
        );

        let c = parse(&[
            "x",
            "--database-url",
            "postgres:///catalyst",
            "--create-writes-enabled",
            "true",
            "--writes-restore-stamp-dir",
            "/var/lib/stamps",
            "--writes-backup-max-age-hours",
            "0",
        ])
        .unwrap();
        assert_eq!(c.restore_stamp_dir(), Some("/var/lib/stamps"));
        let err = c.require_restore_stamps().unwrap_err().to_string();
        assert!(err.contains("WRITES_BACKUP_MAX_AGE_HOURS=0"), "{err}");
        let c = Config {
            writes: super::WritesConfig {
                writes_backup_max_age_hours: 26,
                ..c.writes
            },
            ..c
        };
        c.require_restore_stamps().unwrap();
    }

    #[test]
    fn social_defaults_are_the_old_binaries_values_and_writes_are_off() {
        let cli = parse(&["x", "--database-url", "postgres:///catalyst"]).unwrap();
        let c = cli;
        assert!(
            !c.writes.social_writes_enabled,
            "social writes are opt-in; a restore must be demonstrated first"
        );
        assert!(
            !c.writes.content_writes_enabled,
            "local deploys are opt-in; a restore must be demonstrated first"
        );
        assert!(
            c.writes.content_forward_upstream_url.is_none()
                && c.writes.catalyrst_content_admin_token.is_none(),
            "nothing is forwarded until an operator names an upstream"
        );
        assert_eq!(
            (
                c.writes.forward_max_attempts,
                c.writes.forward_retry_base_secs
            ),
            (12, 5)
        );
        assert!(
            !c.writes.data_writes_enabled,
            "data-plane writes are opt-in, the social switch's twin"
        );
    }
}
