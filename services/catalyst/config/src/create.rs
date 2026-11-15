//! The creator plane's settings (builder and camera_reel).

use clap::Args;

/// The creator plane: builder and camera_reel.
#[derive(Args, Clone, Debug)]
pub struct CreateConfig {
    // create: the creator plane (builder and camera_reel, the old
    // catalyrst-create bundle). Like the social writes, drafts, uploads,
    // curation, publication state and photos are local authority peer sync
    // does not back up, so every creator write answers 503 until a backup
    // restore has been demonstrated (DATABASE.md, "Bootstrap, schema versions and local writes").
    #[arg(long, env = "CREATE_WRITES_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub create_writes_enabled: bool,

    /// Public http(s) URL of the item content bucket; unset, a storage miss
    /// answers 404 instead of the 301 and `/contents/{hash}` serves the store only.
    #[arg(long, env = "BUILDER_CONTENT_BUCKET_URL")]
    pub builder_content_bucket_url: Option<String>,

    /// Public http(s) URL of the asset-packs bucket (scene-template models);
    /// `/contents/{hash}` pulls from it when the item bucket misses.
    #[arg(long, env = "BUILDER_ASSET_PACKS_BUCKET_URL")]
    pub builder_asset_packs_bucket_url: Option<String>,

    /// The Foundation's builder API a linked publication is verified against
    /// (the mirror posture's: Decentraland's); empty answers that route 503.
    #[arg(long, env = "BUILDER_FOUNDATION_URL", default_value = "")]
    pub builder_foundation_url: String,

    /// Polygon JSON-RPC (https to a public host) that verifies publication
    /// receipts; unset, publication begin/claim/verify answer 503.
    #[arg(long, env = "BUILDER_POLYGON_RPC_URL", hide_env_values = true)]
    pub builder_polygon_rpc_url: Option<String>,

    /// Comma-separated admin wallets (curation routes, non-owner collection reads).
    #[arg(long, env = "BUILDER_ADMIN_ADDRESSES", default_value = "")]
    pub builder_admin_addresses: String,

    /// Bearer of the curation routes; unset fails closed.
    #[arg(long, env = "CATALYRST_BUILDER_ADMIN_TOKEN", hide_env_values = true)]
    pub catalyrst_builder_admin_token: Option<String>,

    /// The newsletter forward is configured only when all three are set.
    #[arg(long, env = "NEWSLETTER_SERVICE_URL")]
    pub newsletter_service_url: Option<String>,

    #[arg(long, env = "NEWSLETTER_PUBLICATION_ID")]
    pub newsletter_publication_id: Option<String>,

    #[arg(long, env = "NEWSLETTER_SERVICE_API_KEY", hide_env_values = true)]
    pub newsletter_service_api_key: Option<String>,

    /// Bearer of the moderator routes; unset refuses them.
    #[arg(
        long,
        env = "CATALYRST_CAMERA_REEL_ADMIN_TOKEN",
        hide_env_values = true
    )]
    pub catalyrst_camera_reel_admin_token: Option<String>,

    /// Bearer for this service's metrics endpoint; unset fails closed.
    #[arg(long, env = "CAMERA_REEL_METRICS_BEARER_TOKEN", hide_env_values = true)]
    pub camera_reel_metrics_bearer_token: Option<String>,

    #[arg(long, env = "MAX_IMAGES_PER_USER", default_value_t = 500)]
    pub camera_reel_max_images_per_user: u64,

    /// Cache lifetime for successful world-name lookups; zero disables retention.
    #[arg(
        long,
        visible_alias = "places-cache-ttl-seconds",
        env = "PLACES_CACHE_TTL_SECONDS",
        default_value_t = 300
    )]
    pub camera_reel_places_cache_ttl_seconds: u64,

    /// Maximum world-name lookup entries; zero disables retention.
    #[arg(
        long,
        visible_alias = "places-cache-max-size",
        env = "PLACES_CACHE_MAX_SIZE",
        default_value_t = 1000
    )]
    pub camera_reel_places_cache_max_size: u64,

    /// Base of the rendered `url`/`thumbnailUrl` (old API_URL); empty =
    /// `<PUBLIC_URL>/camera-reel-service`.
    #[arg(long, env = "CAMERA_REEL_API_URL", default_value = "")]
    pub camera_reel_api_url: String,

    /// Public image origin; redirects retain the requested filename.
    /// Use `<CAMERA_REEL_API_URL>/api/files` for the local virtual origin.
    /// An origin serving only blob hashes needs ingress adjustment before cutover.
    /// Without this setting, redirects use the local protected byte route.
    #[arg(long, env = "CAMERA_REEL_BUCKET_URL")]
    pub camera_reel_bucket_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use crate::config::Config;

    fn parse(args: &[&str]) -> Result<Config, clap::Error> {
        Config::parse_for_tests(args)
    }

    #[test]
    fn create_defaults_are_the_old_bundles_values_and_writes_are_off() {
        let cli = parse(&["x", "--database-url", "postgres:///catalyst"]).unwrap();
        let c = cli;
        assert!(
            !c.create.create_writes_enabled,
            "creator writes are opt-in; a restore must be demonstrated first"
        );
        assert!(
            c.create.builder_content_bucket_url.is_none()
                && c.create.builder_asset_packs_bucket_url.is_none()
                && c.create.builder_polygon_rpc_url.is_none()
        );
        assert_eq!(c.create.builder_admin_addresses, "");
        assert!(c.create.catalyrst_builder_admin_token.is_none());
        assert!(
            c.create.newsletter_service_url.is_none()
                && c.create.newsletter_publication_id.is_none()
                && c.create.newsletter_service_api_key.is_none()
        );
        assert!(
            c.create.catalyrst_camera_reel_admin_token.is_none()
                && c.create.camera_reel_metrics_bearer_token.is_none()
                && c.create.camera_reel_bucket_url.is_none()
        );
        assert_eq!(
            (
                c.create.camera_reel_max_images_per_user,
                c.create.camera_reel_api_url.as_str()
            ),
            (500, "")
        );

        let cli = parse(&[
            "x",
            "--database-url",
            "postgres:///catalyst",
            "--create-writes-enabled",
            "true",
            "--builder-admin-addresses",
            "0xABC",
            "--camera-reel-max-images-per-user",
            "7",
        ])
        .unwrap();
        assert!(cli.create.create_writes_enabled);
        assert_eq!(
            (
                cli.create.builder_admin_addresses.as_str(),
                cli.create.camera_reel_max_images_per_user
            ),
            ("0xABC", 7)
        );
    }

    #[test]
    fn camera_places_cache_defaults_and_overrides_match_source() {
        let cli = parse(&["x", "--database-url", "postgres:///catalyst"]).unwrap();
        assert_eq!(cli.create.camera_reel_places_cache_ttl_seconds, 300);
        assert_eq!(cli.create.camera_reel_places_cache_max_size, 1000);
        let upstream_cli = parse(&[
            "x",
            "--database-url",
            "postgres:///catalyst",
            "--places-cache-ttl-seconds",
            "9",
            "--places-cache-max-size",
            "3",
        ])
        .unwrap();
        assert_eq!(upstream_cli.create.camera_reel_places_cache_ttl_seconds, 9);
        assert_eq!(upstream_cli.create.camera_reel_places_cache_max_size, 3);
        use clap::Args;
        let command = super::CreateConfig::augment_args(clap::Command::new("create"));
        for (id, env) in [
            (
                "camera_reel_places_cache_ttl_seconds",
                "PLACES_CACHE_TTL_SECONDS",
            ),
            ("camera_reel_places_cache_max_size", "PLACES_CACHE_MAX_SIZE"),
        ] {
            let arg = command
                .get_arguments()
                .find(|arg| arg.get_id() == id)
                .unwrap();
            assert_eq!(arg.get_env().unwrap(), env);
        }
        for (ttl, size) in [("0", "0"), ("45", "7")] {
            let cli = parse(&[
                "x",
                "--database-url",
                "postgres:///catalyst",
                "--camera-reel-places-cache-ttl-seconds",
                ttl,
                "--camera-reel-places-cache-max-size",
                size,
            ])
            .unwrap();
            assert_eq!(
                cli.create.camera_reel_places_cache_ttl_seconds,
                ttl.parse::<u64>().unwrap()
            );
            assert_eq!(
                cli.create.camera_reel_places_cache_max_size,
                size.parse::<u64>().unwrap()
            );
        }
    }
}
