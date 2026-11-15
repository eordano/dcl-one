//! `catalyst serve`: builds every domain's state and router and runs the HTTP server.
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;

use crate::config::Config;
use crate::content::sync::{SyncSettings, worker};
use crate::social::{badges, communities};
use crate::{
    builder, camera_reel, comms, content, db, explore, http, jobs, land_authz, ops, presence,
    price, shop, storage, worlds,
};

#[path = "serve_lifecycle.rs"]
mod lifecycle;
#[path = "serve_routes.rs"]
mod routes;
#[path = "serve_settings.rs"]
mod settings;
#[path = "serve_states.rs"]
mod states;

#[cfg(test)]
use lifecycle::register_metrics;
pub(crate) use lifecycle::shutdown_signal;
use lifecycle::{install_metrics, install_restore_stamps, run_http};
#[cfg(test)]
pub(crate) use routes::live_presence_routes;
#[cfg(test)]
use routes::registry_profiles;
use routes::{Routes, build_routes};
use settings::*;
use states::*;

#[cfg(test)]
mod worlds_comms_tests {
    use super::worlds_comms_online;
    use crate::shared::livekit::LiveKit;

    fn livekit(configured: bool, client_url: &str) -> LiveKit {
        LiveKit {
            api_key: "key".into(),
            api_secret: "secret".into(),
            client_url: client_url.into(),
            api_url: "https://livekit.example.org".into(),
            configured,
        }
    }

    #[test]
    fn worlds_comms_need_real_credentials_and_a_signaling_url() {
        assert!(worlds_comms_online(&livekit(
            true,
            "wss://livekit.example.org"
        )));
        assert!(
            !worlds_comms_online(&livekit(false, "wss://livekit.example.org")),
            "dev credentials"
        );
        assert!(!worlds_comms_online(&livekit(true, "")), "no signaling URL");
    }
}

#[cfg(test)]
mod metrics_tests {
    #[tokio::test]
    async fn serve_exporter_scrapes_and_rejects_duplicate_startup() {
        // A process-global recorder needs a dedicated child process, so this
        // fixture cannot alter the recorder observed by other root tests.
        const CHILD: &str = "CATALYST_SERVE_METRICS_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "serve::metrics_tests::serve_exporter_scrapes_and_rejects_duplicate_startup",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed"));
            return;
        }
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = reserved.local_addr().unwrap();
        drop(reserved);
        super::install_metrics(&bind.to_string()).unwrap();
        let response = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap()
            .get(format!("http://{bind}/metrics"))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let rendered = response.text().await.unwrap();
        for sample in ["comms_nats_connected 0", "comms_cluster_work_running 0"] {
            assert!(rendered.contains(sample), "missing {sample}: {rendered}");
        }
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let failed_bind = reserved.local_addr().unwrap();
        drop(reserved);
        assert!(super::install_metrics(&failed_bind.to_string()).is_err());
        // Duplicate rejection releases its unspawned HTTP listener. Dependency
        // upkeep lasts until this isolated child's runtime exits; no retry is supported.
        let _reused = std::net::TcpListener::bind(failed_bind).unwrap();
    }

    #[test]
    fn serve_registers_health_and_work_before_events() {
        let recorder = crate::comms::metrics::prometheus_builder()
            .unwrap()
            .build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, super::register_metrics);
        let rendered = handle.render();
        for sample in [
            "comms_nats_connected 0",
            "comms_cluster_events_received_total 0",
            "comms_cluster_work_running 0",
            "comms_cluster_work_queued 0",
            "comms_cluster_work_rejected_total{reason=\"wallet_full\"} 0",
        ] {
            assert!(rendered.contains(sample), "missing {sample}: {rendered}");
        }
    }
}

/// Run with Stats prepared before creating workers. The unsafe preparation
/// contract applies through every route clone, canceled blocking operation and
/// final isolate teardown; an arbitrary preexisting runtime is not supported.
pub async fn serve(
    mut config: Config,
    mut prepared: crate::metrics_matcher::PreparedStats,
) -> anyhow::Result<()> {
    config.node.init_tracing();
    let metrics_handle = install_metrics(&config.node.serve_metrics_bind)?;
    tracing::info!("{}", catalyst_config::posture::startup_line(&config));
    config.resolve_secret_files()?;
    config.content.eth_rpc_url()?;
    install_restore_stamps(&config)?;
    let sync_settings = SyncSettings::from_config(&config)?;
    let pool = db::connect_pool(
        &config.node.database_url,
        &db::PoolSettings {
            max_connections: config.node.pg_pool_size,
            ..Default::default()
        },
    )
    .await
    .context("connecting to DATABASE_URL")?;
    db::bootstrap(&pool).await?;
    worlds::rooms::ensure_world_scene_ban_release(&pool).await?;

    let storage_root = Path::new(&config.node.storage_root_folder);
    let storage = Arc::new(
        storage::Storage::from_config(storage_root)
            .await
            .with_context(|| {
                format!(
                    "opening STORAGE_ROOT_FOLDER {}",
                    config.node.storage_root_folder
                )
            })?,
    );

    // ONE SyncStatus: the orchestrator writes it, /about and /status read it.
    let sync_status = Arc::new(content::SyncStatus::default());
    let content = Arc::new(content::ContentState::with_sync(
        pool.clone(),
        content::Settings::from_config(&config),
        storage.clone(),
        sync_status.clone(),
    ));
    prepared.settings.content_url = stats_content_url(
        config.node.content_url.as_deref(),
        &content.settings.content_public_url,
    );

    // ONE outbound client: peer sync and the RPC validator use it as is
    // (`raw`), every mirror and upstream reader through the guard.
    let http_client = http::Outbound::new(http::HttpClientCfg::serve(
        config.content.connections_max_idle,
    ))
    .context("building the HTTP client")?;

    // Fleet-wide jobs run in one process at a time through leases in the
    // database; an API replica (FLEET_JOBS_ENABLED=false) stands them down.
    let jobs = if config.node.job_leases_enabled {
        jobs::Jobs::with_leases(
            pool.clone(),
            jobs::LeaseSettings {
                namespace: config.node.job_lease_namespace.clone(),
                ..jobs::LeaseSettings::default()
            },
        )
    } else {
        tracing::info!("JOB_LEASES_ENABLED=false: fleet jobs run unconditionally in this process");
        jobs::Jobs::new()
    };
    if !config.node.fleet_jobs_enabled {
        tracing::info!("FLEET_JOBS_ENABLED=false: fleet jobs stand down in this process");
    }
    let jobs = Arc::new(
        jobs.with_fleet_jobs(config.node.fleet_jobs_enabled)
            .with_first_pass_now(config.node.jobs_first_pass_now),
    );
    // Every process mirrors the sync leader's state from the database into its
    // own SyncStatus; the leader (the process holding the `sync` lease) writes it.
    content::sync::status_mirror::spawn_mirror(&jobs, pool.clone(), sync_status.clone());

    // The snapshot generator: the time-windowed files /snapshots lists, so
    // another catalyst can name this node in its SYNC_SOURCE. With the sync
    // worker attached it waits for Syncing; SNAPSHOT_GENERATION_INTERVAL_HOURS=0
    // turns it off (the console's regenerate then answers 501).
    let snapshot_gen = content::snapshot_gen::Settings::from_hours(
        config.content.snapshot_generation_interval_hours,
    )
    .map(|settings| {
        content::snapshot_gen::SnapshotGenerator::new(
            content.clone(),
            sync_settings.is_some(),
            settings,
        )
    });
    match &snapshot_gen {
        Some(generator) => generator.spawn_job(&jobs),
        None => tracing::info!(
            "SNAPSHOT_GENERATION_INTERVAL_HOURS=0: no snapshot generation; /snapshots serves what content.snapshots holds"
        ),
    }

    // The explore, explorer-boot, worlds and marketplace domains: every one
    // over the same pool, client and storage; each mirror runs only when its
    // upstream is configured, and every upstream is checked at startup to be
    // a public endpoint that is not this node.
    let local_urls: Vec<String> = [
        config.node.public_url.clone(),
        config.node.content_url.clone(),
        Some(format!("http://{}", config.node.bind_addr())),
    ]
    .into_iter()
    .flatten()
    .collect();
    let local: Vec<&str> = local_urls.iter().map(String::as_str).collect();

    // ONE EIP-1654 validator (contract-wallet signatures): explore, the
    // social routes and the social socket share it.
    let eip1654: Option<Arc<dyn crate::auth::eip1654::Eip1654Validator>> = config
        .content
        .eth_rpc_url()?
        .map(|url| crate::auth::cached_rpc_validator(url, http_client.raw().clone()));

    // The content validator of the local write path (src/content/validator):
    // the old catalyrst-validator rules over the ONE pool (the escrow
    // overlay), store and client, the shared EIP-1654 validator and the
    // chain access checks over ETH_RPC_URL and RPC_ENDPOINT_POLYGON.
    let validator_settings = content::validator::Settings::parse(
        config.validator.ignore_blockchain_access_checks,
        config.writes.content_writes_enabled,
        config.validator.additional_decentraland_address.as_deref(),
        &config.node.eth_network,
        config.content.eth_rpc_url()?,
        config.economy.rpc_endpoint_polygon.as_deref(),
    )
    .context("content validator configuration")?;
    // Sync and the read-through validate every entity a peer serves.
    let judges_peer_entities = sync_settings.is_some() || config.content.read_through_enabled;
    if judges_peer_entities {
        validator_settings
            .require_chain_for_peer_entities()
            .context("content validator configuration")?;
    }
    let content_validator = Arc::new(content::validator::Sources::new(
        pool.clone(),
        storage.clone(),
        http_client.clone(),
        eip1654.clone(),
        validator_settings,
    ));
    if (config.writes.content_writes_enabled || judges_peer_entities)
        && !config.validator.ignore_blockchain_access_checks
    {
        content_validator
            .verify_chain_ids()
            .await
            .context("deploy access check RPCs")?;
    }

    // READ_THROUGH_ENABLED: a local miss (entities, content files, profiles) is
    // filled from SYNC_SOURCE, verified and stored as a synced deployment; a
    // world not held is mirrored on first read when the worlds mirror is on.
    let read_through = content::read_through::ReadThrough::from_config(
        &config,
        &local_urls,
        pool.clone(),
        storage.clone(),
        http_client.raw().clone(),
        content_validator.clone(),
    )?
    .map(Arc::new);
    if let Some(read_through) = &read_through {
        content.attach_read_through(read_through.clone());
    }
    let source =
        |enabled: bool, url: &str, var: &str| -> anyhow::Result<Option<explore::MirrorSource>> {
            Ok(if enabled {
                Some(explore::MirrorSource::checked(url, &local).context(var.to_string())?)
            } else {
                None
            })
        };
    if config.explore.events_mirror_adopt && config.explore.events_mirror_upstream {
        anyhow::bail!(
            "EVENTS_MIRROR_ADOPT adopts the mirrored events once the mirror is off: unset EVENTS_MIRROR_UPSTREAM first"
        );
    }
    if config
        .node
        .public_url
        .as_deref()
        .is_none_or(|u| u.trim().is_empty())
    {
        tracing::warn!(
            own_url = %config.node.own_url(),
            "PUBLIC_URL is unset: place report upload URLs and asset links use the bind address"
        );
    }
    let explore_settings = explore::Settings {
        content_public_url: content.settings.content_public_url.clone(),
        asset_rewrite_domain: explore::Settings::asset_rewrite_domain_for(
            config.node.public_url.as_deref(),
        ),
        events_admin_token: config.explore.catalyrst_events_admin_token.clone(),
        events_slack_webhook: config.explore.events_slack_webhook.clone(),
        events_upcoming_summaries_enabled: config.explore.events_upcoming_summaries_enabled,
        places_upstream: source(
            config.explore.places_mirror_upstream,
            &config.explore.places_upstream_url,
            "PLACES_UPSTREAM_URL",
        )?,
        places_interval: Duration::from_secs(config.explore.places_mirror_interval_secs.max(1)),
        worlds_upstream: source(
            config.explore.worlds_mirror_upstream,
            &config.explore.worlds_upstream_url,
            "WORLDS_UPSTREAM_URL",
        )?,
        worlds_interval: Duration::from_secs(config.explore.worlds_mirror_interval_secs.max(1)),
        events_upstream: source(
            config.explore.events_mirror_upstream,
            &config.explore.events_upstream_url,
            "EVENTS_UPSTREAM_URL",
        )?,
        events_interval: Duration::from_secs(config.explore.events_mirror_interval_secs.max(1)),
        events_adopt: config.explore.events_mirror_adopt,
        places_derive: Some(explore::places::derive::DeriveScope::resolve(
            config.explore.places_derive_from_content,
            config.explore.places_mirror_upstream,
        )),
        places_derive_interval: Duration::from_secs(
            config.explore.places_derive_interval_secs.max(5),
        ),
        user_writes_enabled: config.writes.data_writes_enabled,
        operator_writes_enabled: config.ops.ops_writes_enabled,
        places_admin_token: config.explore.places_admin_auth_token.clone(),
        data_team_token: config.explore.data_team_auth_token.clone(),
        places_admin_addresses: config
            .explore
            .places_admin_addresses
            .iter()
            .map(|a| a.trim().to_lowercase())
            .filter(|a| !a.is_empty())
            .collect(),
        places_base_url: config.explore.places_base_url.trim().to_string(),
        places_moderation_webhook: config.explore.places_moderation_webhook.clone(),
        snapshot_score_url: Some(config.explore.snapshot_score_url.trim().to_string())
            .filter(|u| !u.is_empty()),
        events_base_url: config
            .explore
            .events_base_url
            .trim_end_matches('/')
            .to_string(),
        jump_in_site_url: config
            .explore
            .jump_in_site_url
            .trim()
            .trim_end_matches('/')
            .to_string(),
        events_asset_base_url: explore::Settings::events_asset_base_for(&config.node.own_url()),
        public_url: config.node.own_url().trim_end_matches('/').to_string(),
        trusted_proxy_cidrs: config.node.http_trusted_proxy_cidrs.clone(),
        realm_name: config
            .node
            .realm_name
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .unwrap_or("main")
            .to_string(),
    };
    let explore_cors = explore::cors::Policy::upstream_with(&config.explore.explore_cors_origins)
        .map_err(|e| anyhow::anyhow!("EXPLORE_CORS_ORIGINS: {e}"))?;
    let explore = Arc::new(
        explore::ExploreState::new(pool.clone(), explore_settings, eip1654.clone())
            .with_storage(storage.clone())
            .with_outbound(http_client.clone())
            .with_cors(explore_cors),
    );
    explore.spawn_jobs(&jobs, http_client.clone());

    // The explorer-api readers and mirrors, the asset-bundle CDN read-through
    // cache (`/ab-cdn`, the old catalyrst-ab-cdn serving half) and the map
    // metadata routes (the old catalyrst-map JSON documents).
    let api_sources = explore::api::Sources::from_config(&config);
    let mut api_settings = explore::api::Settings::from_config(&config, api_sources);
    api_settings.ab_cdn_enabled = config.ab_cdn.ab_cdn_enabled;
    if let Some(u) = Config::non_empty(&config.ab_cdn.map_image_base_url) {
        api_settings.map_image_base_url = u.trim_end_matches('/').to_string();
    }
    api_settings.map_external_base_url = Config::non_empty(&config.ab_cdn.map_external_base_url)
        .map(|u| u.trim_end_matches('/').to_string());
    api_settings.satellite_upstream =
        Config::non_empty(&config.explore.satellite_mirror_upstream_url)
            .map(|u| u.trim_end_matches('/').to_string());
    api_settings.satellite_interval =
        Duration::from_secs(config.explore.satellite_mirror_interval_secs.max(60));
    api_settings.satellite_allow_private = config.explore.satellite_mirror_allow_private_upstream;
    api_settings.satellite_source_budget_bytes =
        config.explore.satellite_source_budget_mb.max(1) << 20;
    api_settings.satellite_output_entries = config.explore.satellite_output_entries.max(1);
    let explorer_api = explore::api::State::new(
        api_settings,
        pool.clone(),
        http_client.clone(),
        content.clone(),
    )
    .await
    .context("starting the explorer-api readers and mirrors")?;
    explorer_api.attach_eip1654(eip1654.clone());
    explorer_api.spawn_jobs(&jobs);
    // An island's /about advertises its own minimap once the grid it draws is loaded.
    let grid = Arc::downgrade(&explorer_api);
    content.attach_parcel_grid(Arc::new(move || {
        grid.upgrade().is_some_and(|api| api.map.is_ready())
    }));

    // The /about comms block (the old COMMS_WS_CONNECTOR_URL/COMMS_STATS_URL
    // probes): archipelago's core status as a module call, advertised when
    // COMMS_FIXED_ADAPTER is set.
    let about_comms = content::about_comms::Settings::resolve(content::about_comms::RawSettings {
        protocol: &config.about_comms.comms_protocol,
        fixed_adapter: config.explore.comms_fixed_adapter.as_deref(),
        version: config.about_comms.comms_version.as_deref(),
        commit_hash: config.about_comms.comms_commit_hash.as_deref(),
        max_users: config.about_comms.max_users,
        offline_when_unreachable: config.about_comms.comms_offline_when_unreachable,
        v4_control_url: config.about_comms.comms_v4_control_url.as_deref(),
        v4_control_audience: config.about_comms.comms_v4_control_audience.as_deref(),
        v4_pulse_native_endpoint: config.about_comms.comms_v4_pulse_native_endpoint.as_deref(),
        v4_pulse_webtransport_url: config
            .about_comms
            .comms_v4_pulse_webtransport_url
            .as_deref(),
        v4_pulse_certificate_hash: config
            .about_comms
            .comms_v4_pulse_certificate_hash
            .as_deref(),
        v4_pulse_audience: config.about_comms.comms_v4_pulse_audience.as_deref(),
        v4_island_refresh_url: config.about_comms.comms_v4_island_refresh_url.as_deref(),
    });
    // Built before worlds so both /about routes share one SFU probe.
    let about_comms =
        about_comms.map(|settings| Arc::new(content::about_comms::AboutCommsState::new(settings)));

    let comms_settings = comms_settings(&config)?;
    let mut worlds_settings = worlds::Settings::from_config(&config)
        .with_global_scenes_urn(config.worlds_content.global_scenes_urn.as_deref());
    worlds_settings.realm_name_strip_ens = config.worlds_content.worlds_realm_name_strip_ens;
    worlds_settings.map_estate_view_url = config.map_urls().estate_view;
    worlds_settings.livekit_client_url = comms_settings.livekit.client_url.clone();
    worlds_settings.comms_online = worlds_comms_online(&comms_settings.livekit);
    if let Some(about) = &about_comms {
        worlds_settings.sfu = about.sfu.clone();
    }
    // Worlds' visitors use this node's content unless CONTENT_URL points elsewhere.
    if Config::non_empty(&config.node.content_url).is_none() {
        worlds_settings.content_readiness =
            worlds::about::ContentReadiness::local(sync_status.clone());
    }
    let worlds_mirror = worlds::MirrorSettings::from_config(&config, &local_urls)?;
    // The live world presence: ONE map the gatekeeper's LiveKit webhook
    // feeds, read by its world mints, its own routes, worlds' status and the
    // presence archive.
    let live_presence = Arc::new(comms::presence::Presence::new());
    let worlds = Arc::new(
        worlds::WorldsState::new(
            pool.clone(),
            storage.clone(),
            worlds_settings,
            worlds_mirror,
            http_client.clone(),
        )
        .with_presence(live_presence.clone()),
    );
    worlds.spawn_jobs(&jobs);

    // The market half of the data plane (README, "Data sources"): the marketplace surface over the subgraph readers and the
    // collections/items mirrors, the LAND authorization index, the economy
    // relay, the MANA spot archive, the rentals store and the presence
    // archive, every one over the same pool, client and job lifecycle. The
    // local writes among them sit behind DATA_WRITES_ENABLED
    // (`shop::market::DataWrites`, the social switch's twin).
    let self_hosts = content::own_hosts(&config);
    let market_settings = crate::settings::market_settings(&config, &self_hosts)?;
    shop::market::catalog::install_served_chains(&market_settings.market.indexes);
    let market = Arc::new(
        shop::market::MarketplaceState::new(
            pool.clone(),
            http_client.clone(),
            market_settings,
        )
        .with_eip1654(eip1654.clone())
        .with_read_through(read_through.clone())
        .with_denylist(storage.denylist().clone())
        .with_feature_flags(shop::market::features::Source {
            url: config.shop.shop_feature_flags_url.clone(),
            origin: if config.shop.shop_feature_flags_origin.is_empty() {
                // The service answers by Referer: an origin it cannot name asks for no site, and every flag reads off.
                match url::Url::parse(&config.shop.shop_base_url).map(|u| u.origin()) {
                    Ok(origin) if origin.is_tuple() => origin.ascii_serialization(),
                    _ => {
                        tracing::warn!(
                            shop_base_url = %config.shop.shop_base_url,
                            "SHOP_FEATURE_FLAGS_ORIGIN is empty and SHOP_BASE_URL names no origin: the dapps flags are asked for no site, so they read off"
                        );
                        String::new()
                    }
                }
            } else {
                config.shop.shop_feature_flags_origin.clone()
            },
            refresh: Duration::from_secs(config.shop.shop_feature_flags_refresh_secs),
        }),
    );
    market.spawn_api_jobs(&jobs);

    let land_settings = crate::settings::land_settings(&config)?;
    let land = Arc::new(land_authz::LandAuthzState::new(
        pool.clone(),
        http_client.clone(),
        land_settings,
        Some(market.index.clone()),
    ));
    explore.places.set_land_authz(land.clone());

    // The content server's local write path (src/content/{create_entity,
    // unpublish_scene,land_publish}): the write deployer over the ONE pool
    // and store with the validator above, the LAND index (the unpublish
    // route's parcel check), the shared EIP-1654 validator and the durable
    // operator denylist (content.denylist, loaded now and reloaded every
    // 60 s), behind CONTENT_WRITES_ENABLED. With CONTENT_FORWARD_UPSTREAM_URL
    // every local deploy is also queued for that catalyst and sent on by the
    // `content-forward` job (src/content/forward.rs).
    let content_forwarder = forwarder(
        "CONTENT_FORWARD_UPSTREAM_URL",
        config.writes.content_forward_upstream_url.as_deref(),
        content::forward::CONTENT_LANE,
        &config,
        &pool,
        &storage,
        &self_hosts,
        &jobs,
    )?;
    let deployer = content::write_deployer::WriteDeployer::new(
        pool.clone(),
        storage.clone(),
        eip1654.clone(),
        content_validator.clone(),
    )
    .with_land_authz(Some(land.clone()))
    .with_denylist(content.denylist.clone())
    .with_forwarder(content_forwarder);
    content.attach_writes(config.writes.content_writes_enabled, Arc::new(deployer));
    // Expired partial uploads (content::partial_deployments) are reclaimed
    // only where they can be made: a node taking local writes.
    if config.writes.content_writes_enabled {
        content::partial_deployments::spawn(
            &jobs,
            pool.clone(),
            storage.clone(),
            content::partial_deployments::Limits::from_config(&config),
        );
    }
    let denylisted = content
        .load_denylist()
        .await
        .context("loading content.denylist")?;
    content.spawn_jobs(&jobs);
    // The remote moderation lists (DENYLIST_URLS; Decentraland's on a
    // mirror), fetched once before the routes serve and every 120 s after,
    // into the same projection; a failed fetch is logged, never fatal.
    let denylist_urls = config.content.denylist_urls()?;
    if !denylist_urls.is_empty() {
        let (remote, errors) = content
            .reload_remote_denylist(&http_client, &denylist_urls)
            .await;
        for error in &errors {
            tracing::warn!(%error, "remote denylist fetch failed at boot");
        }
        tracing::info!(
            urls = denylist_urls.len(),
            remote,
            "remote denylists loaded"
        );
    }
    content.spawn_remote_denylist(&jobs, http_client.clone(), denylist_urls);
    if config.writes.content_writes_enabled {
        tracing::warn!(
            denylisted,
            "CONTENT_WRITES_ENABLED=true: this node is the authority for local deployments and unpublishes"
        );
    } else {
        tracing::info!(
            denylisted,
            "CONTENT_WRITES_ENABLED=false: POST /entities and DELETE /scenes/{{coord}} answer 503"
        );
    }
    // The content history prune (src/content/history_prune.rs): behind
    // CONTENT_HISTORY_PRUNE_ENABLED, and off beside the writes switch (an
    // authoritative node keeps its history); it logs once why it is not running.
    content::history_prune::spawn(
        &jobs,
        pool.clone(),
        storage.clone(),
        content::history_prune::PruneOptions::from_config(&config),
    );

    if config.shop.credits_trial_mode {
        shop::credits::trial::refuse_a_shared_key(
            config.shop.credits_revoker_key.as_deref(),
            config.economy.relayer_private_key.as_deref(),
        )?;
    }
    let economy_settings = shop::evm::Settings {
        min_sale_value_in_wei: config.economy.min_sale_value_in_wei.clone(),
        max_transactions_per_day: config.economy.max_transactions_per_day,
        max_relayed_transactions_per_day: config.economy.max_relayed_transactions_per_day,
        contract_addresses_url: Config::non_empty(&config.economy.contract_addresses_url),
        contract_addresses_chain_key: config.economy.contract_addresses_chain_key.clone(),
        collections_chain_id: config.economy.collections_chain_id,
        collections_fetch_interval: Duration::from_millis(
            config.economy.collections_fetch_interval_ms,
        ),
        polygon_rpc_url: Config::non_empty(&config.economy.rpc_endpoint_polygon),
        max_gas_price_allowed_in_wei: config.economy.max_gas_price_allowed_in_wei,
        max_gas_limit: config.economy.max_gas_limit,
        relayer_max_fee_per_gas_wei: config.economy.relayer_max_fee_per_gas_wei,
        trial_mode: config.shop.credits_trial_mode,
        // The same list, parsed the same way, as the credits lane's (credits::Settings::parse).
        trial_buyers: config
            .shop
            .credits_trial_mode
            .then(|| {
                shop::credits::trial::TrialBuyers::parse(
                    config.shop.credits_trial_buyers.as_deref(),
                )
            })
            .transpose()?,
        oz_relayer_url: Config::non_empty(&config.economy.oz_relayer_url),
        oz_relayer_id: Config::non_empty(&config.economy.oz_relayer_id),
        oz_relayer_api_key: Config::non_empty(&config.economy.oz_relayer_api_key),
        oz_relayer_speed: config.economy.oz_relayer_speed.clone(),
        oz_max_status_checks: config.economy.oz_max_status_checks,
        oz_sleep_between_checks: Duration::from_millis(
            config.economy.oz_sleep_time_between_checks_ms,
        ),
        meta_tx_broadcast_enabled: config.economy.meta_tx_broadcast_enabled,
        relayer_private_key: Config::non_empty(&config.economy.relayer_private_key),
        transactions_upstream_url: Config::non_empty(&config.economy.transactions_upstream_url),
        transactions_upstream_timeout: Duration::from_millis(
            config.economy.transactions_upstream_timeout_ms,
        ),
        admin_token: Config::non_empty(&config.economy.catalyrst_economy_admin_token),
        landiler_escrow_address: Config::non_empty(&config.economy.landiler_escrow_address),
        names_chain_id: config.economy.names_chain_id,
        eth_rpc_url: config.content.eth_rpc_url()?,
        names_max_price_wei: Config::non_empty(&config.economy.names_max_price_wei)
            .map(|s| alloy_primitives::U256::from_str_radix(s.trim(), 10))
            .transpose()
            .context("NAMES_MAX_PRICE_WEI")?,
        receipt_poll_interval: Duration::from_millis(config.economy.receipt_poll_interval_ms),
        receipt_timeout: Duration::from_millis(config.economy.receipt_timeout_ms),
        broker_reconcile_interval: Duration::from_millis(
            config.economy.broker_reconcile_interval_ms,
        ),
        mana_usd_aggregator: Config::non_empty(&config.economy.mana_usd_aggregator_address)
            .map(|s| s.trim().parse())
            .transpose()
            .context("MANA_USD_AGGREGATOR_ADDRESS")?
            .unwrap_or(shop::evm::oracle::MANA_USD_AGGREGATOR_POLYGON),
        usd_pegged_oracle_max_age_secs: config.economy.usd_pegged_oracle_max_age_secs,
        usd_pegged_slippage_bps: config.economy.usd_pegged_slippage_bps,
        data_writes_enabled: config.writes.data_writes_enabled,
        ops_writes_enabled: config.ops.ops_writes_enabled,
    };
    // Names the variable, never echoes a value.
    economy_settings
        .validate()
        .context("economy configuration")?;
    let economy_state = Arc::new(
        shop::evm::State::new(pool.clone(), http_client.clone(), economy_settings)
            .context("provisioning the economy relayers")?,
    );
    shop::checkout::spawn_jobs(&economy_state, &jobs);

    let price = Arc::new(price::State::new(
        pool.clone(),
        http_client.clone(),
        price::Settings::resolve(
            price::RawSettings {
                poll_enabled: config.price.price_poll_enabled,
                coingecko_url: config.price.coingecko_url.clone(),
                poll_interval_secs: config.price.price_poll_interval_secs,
                fallback_sources: config.price.price_fallback_sources.clone(),
                admin_token: Config::non_empty(&config.price.price_admin_token),
                writes_enabled: config.writes.data_writes_enabled,
            },
            &self_hosts,
        )
        .context("PRICE_* / COINGECKO_URL configuration")?,
    ));
    price.spawn_jobs(&jobs);

    let signatures = Arc::new(shop::rentals::State::new(
        pool.clone(),
        Some(market.clone()),
        shop::rentals::Settings {
            writes_enabled: config.writes.data_writes_enabled,
        },
        eip1654.clone(),
    ));

    tracing::info!(
        mirrors = config.marketplace.marketplace_mirrors_enabled,
        collections_networks = market.settings.collection_networks().len(),
        marketplace_subgraph = market.settings.marketplace_subgraph_url.is_some(),
        trade_rpc_chains = market.settings.market.trade_rpc_urls.iter().count(),
        market_admin = market.settings.market.admin_token.is_some(),
        land_index = land.settings.is_configured(),
        relay_mode = economy_state.settings.relay_mode(),
        economy_admin = economy_state.settings.admin_token.is_some(),
        price_poll = config.price.price_poll_enabled,
        rentals_reader = signatures.reader_configured(),
        "data-plane routes (market half)"
    );
    if config.writes.data_writes_enabled {
        tracing::warn!(
            "DATA_WRITES_ENABLED=true: this node is the authority for signed trades, lists, relays, rentals and the presence archive"
        );
    } else {
        tracing::info!(
            "DATA_WRITES_ENABLED=false: data-plane routes serve reads; every write answers 503"
        );
    }

    // The creator plane (the old catalyrst-create bundle): the builder and the
    // camera reel over the same pool, client, blob store and validator, both
    // behind CREATE_WRITES_ENABLED (builder::writes).
    let builder = Arc::new(
        builder::State::new(
            pool.clone(),
            storage.clone(),
            content.denylist.clone(),
            http_client.clone(),
            Some(market.index.clone()),
            eip1654.clone(),
            builder::Settings::parse(builder::RawSettings {
                writes_enabled: config.create.create_writes_enabled,
                content_bucket_url: config.create.builder_content_bucket_url.clone(),
                asset_packs_bucket_url: config.create.builder_asset_packs_bucket_url.clone(),
                polygon_rpc_url: config.create.builder_polygon_rpc_url.clone(),
                admin_addresses: config.create.builder_admin_addresses.clone(),
                admin_token: config.create.catalyrst_builder_admin_token.clone(),
                newsletter_service_url: config.create.newsletter_service_url.clone(),
                newsletter_publication_id: config.create.newsletter_publication_id.clone(),
                newsletter_api_key: config.create.newsletter_service_api_key.clone(),
                foundation_url: Some(config.create.builder_foundation_url.clone()),
            })
            .context("BUILDER_* / NEWSLETTER_* configuration")?,
        )
        .context("builder state")?,
    );
    let public_url = config
        .node
        .public_url
        .clone()
        .unwrap_or_else(|| format!("http://{}", config.node.bind_addr()));
    let camera_reel = Arc::new(camera_reel::State::new(
        pool.clone(),
        storage.clone(),
        camera_reel::Settings {
            api_url: Config::non_empty(&Some(config.create.camera_reel_api_url.clone()))
                .unwrap_or_else(|| {
                    format!("{}/camera-reel-service", public_url.trim_end_matches('/'))
                }),
            bucket_url: Config::non_empty(&config.create.camera_reel_bucket_url),
            max_images_per_user: config.create.camera_reel_max_images_per_user,
            admin_token: Config::non_empty(&config.create.catalyrst_camera_reel_admin_token),
            writes_enabled: config.create.create_writes_enabled,
            metrics_bearer_token: config.create.camera_reel_metrics_bearer_token.clone(),
            places_cache_ttl_seconds: config.create.camera_reel_places_cache_ttl_seconds,
            places_cache_max_size: config.create.camera_reel_places_cache_max_size,
        },
        eip1654.clone(),
    ));
    let withheld = camera_reel
        .load_withheld()
        .await
        .context("loading camera_reel.withheld_blobs")?;
    camera_reel.spawn_ranking_job(&jobs);
    camera_reel::events::spawn_retention_job(&camera_reel, &jobs);
    tracing::info!(
        builder = %builder.sources_label(),
        camera_reel_api_url = %camera_reel.settings.api_url,
        camera_reel_admin = camera_reel.settings.admin_token.is_some(),
        withheld_blobs = withheld,
        "creator routes"
    );
    if config.create.create_writes_enabled {
        tracing::warn!("CREATE_WRITES_ENABLED=true: this node is the authority for creator writes");
    } else {
        tracing::info!(
            "CREATE_WRITES_ENABLED=false: creator routes serve reads; every write answers 503"
        );
    }

    // The operator plane and the remaining services (the old
    // catalyrst-credits, -rpc and -bvimposters units): every one over the
    // same pool, client, blob store, validator and job lifecycle. The credits
    // ledger is local authority behind OPS_WRITES_ENABLED (the social
    // switch's twin).
    let stage5_ops = ops_states(
        &config,
        &pool,
        &http_client,
        &jobs,
        OpsSources {
            market: &market,
            price: &price,
            economy: &economy_state,
            eip1654: eip1654.clone(),
            public_url: &public_url,
        },
    )
    .await?;

    // The worlds permission core: ONE instance for the worlds product
    // surface, the storage routes, the scene host, archipelago's ban check
    // and comms (the local call that replaced the old permission-document
    // HTTP hops). Its name authority is the marketplace reader.
    let personal_worlds = worlds::policy::PersonalWorldsPolicy::new(
        config.worlds.worlds_personal_worlds_max,
        config.worlds.worlds_personal_world_max_size_bytes,
        worlds::policy::MAX_SCENE_SIZE_BYTES,
    )
    .context("WORLDS_PERSONAL_WORLDS_MAX / WORLDS_PERSONAL_WORLD_MAX_SIZE_BYTES")?;
    let land_operators = comms_settings
        .land_operators_url
        .as_deref()
        .map(|url| worlds::policy::LandOperatorsReader::new(http_client.clone(), url));
    let ens = if config.worlds.allow_ens_domains {
        let rpc = config.content.eth_rpc_url().context("ETH_RPC_URL")?;
        Some(Arc::new(worlds::ens::EnsResolver::configured(
            http_client.clone(),
            rpc.as_deref(),
            &config.node.eth_network,
        )?))
    } else {
        None
    };
    let ban_status: Arc<dyn comms::gatekeeper::moderation::BanStatus> =
        Arc::new(comms::gatekeeper::moderation::Moderation::new(pool.clone()));
    let policy = Arc::new(
        worlds::policy::Policy::new(pool.clone(), personal_worlds, Some(market.index.clone()))
            .with_ban_status(ban_status.clone())
            .with_land_operators(land_operators)
            .with_land_authz(Some(land.clone()))
            .with_max_parcels(config.worlds.max_parcels)
            .with_ens(ens, i64::from(config.worlds.ens_max_size) * 1024 * 1024)
            .with_name_deny_list(worlds.names.clone()),
    );
    worlds.attach_policy(policy.clone());
    // A world read-through holds each mirrored scene's signer to this policy and shares the
    // content read-through's download bound.
    if let Some(read_through) = &read_through {
        worlds.attach_read_through(worlds::WorldReadThrough {
            fills: content::read_through::Fills::new(
                content::read_through::MAX_CONCURRENT_FILLS,
                content::read_through::DEFAULT_WAIT,
            ),
            downloads: read_through.downloads().clone(),
            policy: policy.clone(),
        });
    }
    let stage5 = worlds_states(
        &config,
        &pool,
        &storage,
        &http_client,
        &policy,
        eip1654.clone(),
        &local_urls,
        &jobs,
        live_presence.clone(),
    )
    .await?;
    // One process holds every connector socket: the primary mounts /ws, /ws/v4
    // and the routes that read them, and runs the island subscriber that mints
    // for them; an API replica keeps the feed job (/core-status, /about).
    let primary = config.node.fleet_jobs_enabled;
    if primary {
        tracing::info!(
            "FLEET_JOBS_ENABLED=true: this process serves /ws, /ws/v4 and the socket-reading connector routes and runs the island subscriber; run exactly one such process per node"
        );
    } else {
        tracing::info!(
            "FLEET_JOBS_ENABLED=false: this process serves no /ws, /ws/v4 or socket-reading connector route and mints no island; the primary serves them"
        );
    }
    // The gatekeeper's room events feed the worlds access log and, when
    // enabled, the presence badges (stored before the webhook is
    // acknowledged, applied by a leased job).
    let presence_badges =
        config.social.badges_presence_enabled && config.writes.social_writes_enabled;
    let room_event_sinks: Vec<Arc<dyn comms::gatekeeper::events::RoomEventSink>> =
        vec![stage5.core.clone()];
    let presence_badge_settings = {
        let social = &config.social;
        let ms = badges::presence::secs_to_ms;
        badges::presence::Settings {
            check_after_ms: ms(social.badges_presence_open_visit_check_after_seconds),
            max_age_ms: ms(social.badges_presence_open_visit_max_age_seconds),
            applied_events_ttl_ms: ms(social.badges_presence_applied_events_ttl_seconds),
            absent_expire_ms: ms(social.badges_presence_absent_expire_after_seconds),
            lookahead_ms: ms(social.badges_presence_early_join_lookahead_seconds),
            sweep_batch: i64::from(social.badges_presence_sweep_batch_size),
            prune_batch: i64::from(social.badges_presence_prune_batch_size),
            sweep_interval: Duration::from_secs(social.badges_presence_sweep_interval_seconds),
            grant_notifications: social.badges_presence_grant_notifications,
            sweeper_enabled: social.badges_presence_sweeper_enabled,
            ..badges::presence::Settings::default()
        }
    };
    let presence_badge_store = presence_badges.then(|| {
        Arc::new(badges::presence::EventSink::new(
            pool.clone(),
            presence_badge_settings.clone(),
        )) as Arc<dyn comms::gatekeeper::events::RoomEventStore>
    });
    let comms_stage = comms_states(
        &config,
        &pool,
        &http_client,
        &market,
        comms_settings,
        &policy,
        ban_status.clone(),
        Arc::new(comms::gatekeeper::events::RoomEventFanout::new(
            room_event_sinks,
        )),
        presence_badge_store,
        live_presence.clone(),
        eip1654.clone(),
        primary,
        &jobs,
    )
    .await?;

    match about_comms {
        Some(about) => {
            about.attach_core(comms_stage.archipelago.statistics.clone());
            about.spawn_jobs(&jobs, http_client.raw().clone());
            tracing::info!(adapter = %about.settings.fixed_adapter, sfu_probe = %about.sfu.target(), "/about advertises comms");
            content.attach_about_comms(about);
        }
        None => tracing::info!("COMMS_FIXED_ADAPTER unset: /about carries no comms block"),
    }

    let outfits = Arc::new(shop::outfits::State::new(
        pool.clone(),
        storage.clone(),
        shop::outfits::Settings {
            creators: shop::outfits::Settings::parse_creators(&config.shop.shop_outfit_creators),
            writes_enabled: config.writes.data_writes_enabled,
        },
        eip1654.clone(),
    ));
    let mana = Arc::new(
        shop::mana::State::new(
            pool.clone(),
            http_client.clone(),
            market.clone(),
            signatures.clone(),
            shop::mana::Settings {
                writes_enabled: config.writes.data_writes_enabled,
                track_interval: Duration::from_secs(
                    config.marketplace.marketplace_index_interval_secs,
                ),
                collections_chain_id: config.economy.collections_chain_id,
            },
            eip1654.clone(),
        )
        .context("the Shop's MANA settlement venues")?,
    );
    shop::mana::spawn_jobs(&mana, &jobs);
    // The Shop prices at the settlement chain's newest marketplace's own feed
    // (`shop::mana::feed_rate`), read in the background; without an endpoint or a
    // marketplace on that chain it keeps the spot.
    let settlement = mana.settings.collections_chain_id;
    match shop::mana::feed_rate::settlement_venue(&mana.venues, settlement) {
        Some((url, marketplace)) => shop::mana::feed_rate::install(
            Arc::new(
                shop::credits::feed::ChainFeed::new(
                    Arc::new(shop::evm::chain::ChainComponent::new(
                        http_client.clone(),
                        &url,
                        shop::evm::chain::DEFAULT_RPC_TIMEOUT,
                    )),
                    config.shop.credits_usd_pegged_feed_max_age_secs,
                )
                .reading_past_tolerance(),
            ),
            settlement,
            marketplace,
            config.shop.credits_usd_pegged_feed_max_age_secs,
        ),
        None => tracing::info!(
            chain_id = settlement,
            "shop MANA/USD: no endpoint or marketplace for the settlement chain; the Shop prices at the market spot"
        ),
    }
    let social = social_states(
        &config,
        &pool,
        &storage,
        &http_client,
        &content,
        &market,
        comms_stage.gatekeeper.clone(),
        eip1654,
        &explore,
        explorer_api.onboarding.clone(),
        metrics_handle,
    )?;
    social
        .rpc
        .start_topology(&jobs, comms_stage.archipelago.feed.topology.clone());
    social
        .rpc
        .start_fanout(&jobs, comms_stage.gatekeeper.bus.clone());
    market.attach_profile_images(social.profile_images.clone());
    let subscriptions = Arc::new(shop::subscriptions::State::new(
        pool.clone(),
        Arc::new(social.notifications.email.clone()),
        shop::subscriptions::Settings {
            shop_base_url: config.shop.shop_base_url.clone(),
            delivery_interval: Duration::from_secs(config.shop.shop_notify_delivery_interval_secs),
            watch_interval: Duration::from_secs(config.shop.shop_notify_watch_interval_secs),
            writes_enabled: config.writes.data_writes_enabled,
        },
        outfits.eip1654.clone(),
    ));
    subscriptions.spawn_jobs(&jobs);
    shop::coupons::favorites::spawn_job(&market, config.shop.shop_base_url.clone(), &jobs);
    explore
        .live
        .set_participants(comms_stage.gatekeeper.clone());
    social
        .communities
        .set_voice_eviction(comms_stage.gatekeeper.clone());
    social.communities.set_chain_index(market.index.clone());
    social.communities.load_flags().await?;
    if let Err(error) = social.communities.refresh_flags(&explorer_api).await {
        tracing::warn!(%error,"initial DAPPS flags refresh failed; retaining the stored document");
    }
    social
        .communities
        .spawn_flags_job(&jobs, explorer_api.clone());
    social
        .communities
        .referral
        .set_denylist(explorer_api.denylist.clone());
    communities::ranking::spawn_jobs(&social.communities, &jobs);
    communities::compliance::spawn_job(&social.communities, &jobs);
    // A permission, access-setting or block edit reaches the running rooms
    // at once: the host's sockets and comms' LiveKit participants.
    if let Some(host) = &stage5.scene_host {
        stage5.core.add_revocation_sink(host.clone());
    }
    stage5
        .core
        .add_revocation_sink(Arc::new(worlds::core::CommsRoomRevocations(
            comms_stage.gatekeeper.clone(),
        )));
    // A community exit re-decides admission to the worlds listing it.
    social
        .communities
        .set_world_access_revocation(Arc::new(WorldsCommunityAccess(stage5.core.clone())));
    // The presence archive reads archipelago, comms and the worlds core as
    // module calls (the old ARCHIPELAGO_URL / COMMS_URL / WORLDS_SERVER_URL hops).
    let presence = Arc::new(presence::PresenceState::new(
        pool.clone(),
        presence::Settings::from_switches(
            config.presence.presence_collector_enabled,
            config.writes.data_writes_enabled,
        )
        .with_knobs(
            config.presence.presence_snapshot_interval_secs,
            Some(&config.presence.presence_genesis_realm),
            config.presence.presence_retention_days,
        ),
        presence::sources::ModuleSources::new(
            comms_stage.archipelago.statistics.clone(),
            Arc::new(comms::stats::content::Content {
                pool: pool.clone(),
                denylist: content.denylist.clone(),
            }),
            prepared.settings.content_url.clone(),
            comms_stage.gatekeeper.clone(),
            stage5.core.clone(),
        ),
    ));
    presence.spawn_jobs(&jobs);
    social.notifications.spawn_jobs(&jobs);
    let catalog = social.badges.clone();
    jobs.spawn("badges-catalog-check", move |ctx| async move {
        tokio::select! {
            _ = ctx.cancel.cancelled() => Ok(()),
            checked = catalog.badges.warn_ahead_of_code() => {
                checked.map(drop).context("badge catalog check failed")
            }
        }
    });
    comms_stage.gatekeeper.spawn_jobs(&jobs);
    social.media.spawn_jobs(&jobs);
    social.rpc.spawn_jobs(&jobs);
    social.segment_webhooks.spawn_jobs(&jobs);
    if presence_badges {
        badges::presence::spawn_jobs(
            pool.clone(),
            social.badges.badges.asset_base_url().to_owned(),
            live_presence.clone(),
            presence_badge_settings,
            &jobs,
        );
    }
    if config.writes.social_writes_enabled {
        badges::jobs::spawn(&social.badges, &jobs);
        let communities = social.communities.clone();
        jobs.spawn_periodic_leased(
            "social-referral-rewards",
            jobs::PeriodicCfg::every(Duration::from_secs(60)),
            move |_ctx| {
                let communities = communities.clone();
                async move {
                    communities
                        .referral
                        .drain_pending_rewards()
                        .await
                        .map_err(|error| {
                            anyhow::anyhow!("referral reward drain failed: {error:?}")
                        })?;
                    Ok(())
                }
            },
        );
    }
    communities::fed::spawn_job(&social.communities, &jobs);
    // Copies of the upstream public communities; not local authority, so
    // not behind SOCIAL_WRITES_ENABLED (the explore mirrors' rule). A join or
    // leave is a local membership; every other write on them answers 409
    // naming the upstream (communities::mirror::refuse_mirrored_writes).
    if let Some(upstream) = source(
        config.social.communities_mirror_upstream,
        &config.social.communities_upstream_url,
        "COMMUNITIES_UPSTREAM_URL",
    )? {
        let thumbnail_base =
            explore::MirrorSource::checked(&config.social.communities_mirror_thumbnail_url, &local)
                .context("COMMUNITIES_MIRROR_THUMBNAIL_URL")?;
        social.communities.set_mirror_source(upstream.base());
        communities::mirror::spawn_job(
            social.communities.clone(),
            http_client.clone(),
            communities::mirror::MirrorConfig {
                source: upstream,
                thumbnail_base,
                interval: Duration::from_secs(
                    config.social.communities_mirror_interval_secs.max(1),
                ),
            },
            &jobs,
        );
    }
    if config.writes.social_writes_enabled {
        tracing::warn!("SOCIAL_WRITES_ENABLED=true: this node is the authority for social writes");
    } else {
        tracing::info!(
            "SOCIAL_WRITES_ENABLED=false: social routes serve reads; every write answers 503"
        );
    }
    if config.worlds.worlds_writes_enabled {
        tracing::warn!(
            "WORLDS_WRITES_ENABLED=true: this node is the authority for world publishes, permissions and storage"
        );
    } else {
        tracing::info!(
            "WORLDS_WRITES_ENABLED=false: worlds routes serve reads; every write answers 503"
        );
    }

    // The orchestrator's pause/resume/force handle goes to the operator
    // console; without a sync worker its controls answer 501.
    let sync_control = match &sync_settings {
        Some(settings) => Some(
            worker::start(
                settings,
                db::connect_pool(
                    &config.node.database_url,
                    &db::PoolSettings {
                        max_connections: config.node.sync_pool_size(),
                        ..Default::default()
                    },
                )
                .await
                .context("connecting the sync pool to DATABASE_URL")?,
                storage.clone(),
                sync_status,
                http_client.raw().clone(),
                content::sync_deployer::SyncValidation::Content(content_validator.clone()),
                &jobs,
            )
            .await
            .context("starting the sync worker")?
            .control,
        ),
        None if config.node.serve_unsynced => {
            tracing::warn!(
                "--serve-unsynced: reporting Syncing with no sync worker attached; \
                 development only, never a readiness signal"
            );
            worker::serve_unsynced(&config, &sync_status);
            worker::spawn_startup_repair(&jobs, pool.clone());
            None
        }
        None => {
            tracing::info!(
                "SYNC_ENABLED=false: no sync worker; the node reports the sync leader's state from the database, Bootstrapping until one has written it"
            );
            worker::spawn_startup_repair(&jobs, pool.clone());
            None
        }
    };
    // With sync off the read-through's undecided rows still need the retry and prune loops.
    if sync_settings.is_none()
        && let Some(read_through) = &read_through
    {
        worker::start_failed_deployments(
            &worker::FailedDeploymentSettings::from_config(&config),
            read_through.sources().to_vec(),
            pool.clone(),
            storage.clone(),
            http_client.raw().clone(),
            content::sync_deployer::SyncValidation::Content(content_validator.clone()),
            &jobs,
        );
    }

    // The operator console (the old catalyrst-live /admin API): SIWE sign-in
    // over ADMIN_ADDRESSES + SESSION_SECRET, the sync controls, the cache
    // flush, the failed-deployments re-arm/purge, the job/cache/sync/mirror
    // reports and the ops.admin_audit log.
    let abgen = Config::non_empty(&config.admin.abgen_admin_url)
        .map(|url| {
            ops::admin::console::Abgen::parse(
                &url,
                Config::non_empty(&config.admin.ab_registry_admin_token),
                &http_client,
            )
        })
        .transpose()
        .context("ABGEN_ADMIN_URL")?;
    let ops_admin = Arc::new(
        ops::admin::State::new(
            pool.clone(),
            ops::admin::Settings::parse(ops::admin::RawSettings {
                session_secret: Config::non_empty(&config.admin.session_secret),
                admin_addresses: config.admin.admin_addresses.clone(),
                session_ttl_secs: config.admin.admin_session_ttl_secs,
                cookie_insecure: config.admin.admin_cookie_insecure,
                eth_network: config.node.eth_network.clone(),
                writes_enabled: config.ops.ops_writes_enabled,
            }),
            content.clone(),
            jobs.clone(),
            sync_control,
        )
        // The console's /admin/api/<family>/* calls into these modules in
        // this process (the old console proxied to each over loopback HTTP).
        .with_domains(ops::admin::console::Domains {
            credits: Some(stage5_ops.credits.clone()),
            price: Some(price.clone()),
            comms: Some(comms_stage.gatekeeper.clone()),
            social_rpc: Some(social.rpc.clone()),
            communities: Some(social.communities.clone()),
            notifications: Some(social.notifications.clone()),
            badges: Some(social.badges.clone()),
            worlds: Some(stage5.core.clone()),
            scene_host: stage5.scene_host.clone(),
            camera_reel: Some(camera_reel.clone()),
            builder: Some(builder.clone()),
            rpc: Some(stage5_ops.rpc.clone()),
            explorer_api: Some(explorer_api.clone()),
            explore: Some(explore.clone()),
            abgen,
            social_writes_enabled: config.writes.social_writes_enabled,
            worlds_writes_enabled: stage5.writes_enabled,
        }),
    );
    tracing::info!(
        console = ops_admin.settings.admin_enabled(),
        sync_control = ops_admin.sync_control.is_some(),
        credits_admin = stage5_ops.credits.settings.admin_token.is_some(),
        stripe = stage5_ops.credits.settings.stripe_secret_key.is_some(),
        rpc_networks = stage5_ops.rpc.upstreams_snapshot().len(),
        rpc_admin = stage5_ops.rpc.settings.admin_token.is_some(),
        imposters_bake = stage5_ops.imposters.settings.bake.is_some(),
        abgen_admin = ops_admin.domains.abgen.is_some(),
        "operator-plane routes"
    );
    if config.ops.ops_writes_enabled {
        tracing::warn!(
            "OPS_WRITES_ENABLED=true: this node is the authority for the credits ledger and the operator console's writes"
        );
    } else {
        tracing::info!(
            "OPS_WRITES_ENABLED=false: operator-plane routes serve reads; every write answers 503"
        );
    }
    let addr = config.node.bind_addr();
    let connector_lifecycle = comms_stage.archipelago.clone();
    let app = build_routes(
        &config,
        prepared,
        primary,
        Routes {
            content,
            market,
            stage5_ops,
            economy_state,
            signatures,
            outfits,
            subscriptions,
            mana,
            land,
            price,
            presence,
            explore,
            worlds,
            stage5: &stage5,
            live_presence,
            explorer_api,
            archipelago_state: comms_stage.archipelago,
            gatekeeper: &comms_stage.gatekeeper,
            social: &social,
            builder,
            camera_reel,
            ops_admin,
            snapshot_gen,
        },
    );
    run_http(addr, app, connector_lifecycle, &jobs).await
}

/// The base of `/hot-scenes` thumbnails, the server the scenes are read from, as
/// upstream Stats uses one URL for both. An operator's `CONTENT_URL` is kept as
/// written, since upstream concatenates `${url}/contents/` literally. This node's
/// own `/content/` loses its trailing slash, so `{base}/contents/{hash}` is a
/// route the node serves (the nested `/content` route does not match `//`).
fn stats_content_url(configured: Option<&str>, content_public_url: &str) -> String {
    match configured.map(str::trim).filter(|url| !url.is_empty()) {
        Some(url) => url.to_string(),
        None => content_public_url.trim_end_matches('/').to_string(),
    }
}

#[cfg(test)]
mod stats_content_url_tests {
    use super::stats_content_url;

    #[test]
    fn the_node_default_names_a_served_route_and_an_operator_value_is_kept() {
        assert_eq!(
            stats_content_url(None, "https://node.example/content/"),
            "https://node.example/content"
        );
        assert_eq!(
            stats_content_url(Some("  "), "https://node.example/content/"),
            "https://node.example/content"
        );
        assert_eq!(
            stats_content_url(
                Some("https://cdn.example/content/"),
                "https://cdn.example/content/"
            ),
            "https://cdn.example/content/"
        );
    }
}

#[cfg(test)]
mod registry_mount_tests {
    use std::sync::Arc;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::registry_profiles;
    use crate::content::{ContentState, Settings as ContentSettings};
    use crate::http::Outbound;

    /// Both halves of `/asset-bundle-registry` as `serve` composes them:
    /// building the app panics on an overlapping route, and each half still
    /// answers its own paths.
    #[tokio::test]
    async fn the_registry_prefix_serves_profiles_and_gateway_reads_together() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        let files = std::env::temp_dir().join(format!(
            "catalyst-registry-mount-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let storage = Arc::new(crate::storage::Storage::from_config(&files).await.unwrap());
        let content = Arc::new(ContentState::new(
            pool.clone(),
            ContentSettings::for_tests(),
            storage,
        ));
        let sources = crate::explore::api::Sources {
            registry: String::new(),
            ..crate::explore::api::Sources::defaults()
        };
        let mut settings = crate::explore::api::Settings::for_tests(sources);
        settings.mirrors_enabled = false;
        settings.ab_cdn_enabled = false;
        settings.ab_cdn_cache_dir = files.join("ab-cdn");
        let api =
            crate::explore::api::State::new(settings, pool.clone(), Outbound::for_tests(), content)
                .await
                .unwrap();
        let market = Arc::new(crate::shop::market::MarketplaceState::new(
            pool,
            Outbound::for_tests(),
            crate::shop::market::Settings::for_tests(),
        ));
        // `serve` merges both halves into an app with a fallback of its own
        // (the content 404); the Registry prefix keeps the gateway's.
        let app = axum::Router::new()
            .fallback(|| async { (StatusCode::IM_A_TEAPOT, "app fallback") })
            .merge(registry_profiles(market))
            .merge(crate::explore::api::router(api));
        for (method, path, body, expected) in [
            (
                "POST",
                "/asset-bundle-registry/profiles",
                r#"{"ids":[]}"#,
                json!([]),
            ),
            (
                "POST",
                "/asset-bundle-registry/profiles/metadata",
                r#"{"ids":[]}"#,
                json!([]),
            ),
            (
                "POST",
                "/asset-bundle-registry/entities/active",
                r#"{"pointers":["0,0"]}"#,
                json!([]),
            ),
            (
                "POST",
                "/asset-bundle-registry/entities/versions",
                r#"{"pointers":["0,0"]}"#,
                json!([]),
            ),
            (
                "GET",
                "/asset-bundle-registry/health",
                "",
                json!({"status": "ok"}),
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{method} {path}");
            let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap(),
                expected,
                "{method} {path}"
            );
        }
        // The gateway half's status-check probes, and the Registry's own CORS
        // preflight on a route of each half.
        for (path, expected) in [
            ("/asset-bundle-registry/health/live", "alive"),
            (
                "/asset-bundle-registry/health/ready",
                crate::shared::health::SERVER_PASS,
            ),
            (
                "/asset-bundle-registry/health/startup",
                crate::shared::health::SERVER_PASS,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
            assert_eq!(body.as_ref(), expected.as_bytes(), "{path}");
        }
        for path in [
            "/asset-bundle-registry/profiles/metadata",
            "/asset-bundle-registry/entities/active",
            // A path neither half routes: the gateway half's fallback.
            "/asset-bundle-registry/nope",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("OPTIONS")
                        .uri(path)
                        .header("origin", "https://example.org")
                        .header("access-control-request-method", "POST")
                        .header("access-control-request-headers", "content-type")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT, "{path}");
            let headers = response.headers();
            assert_eq!(headers["access-control-allow-origin"], "*", "{path}");
            assert_eq!(
                headers["access-control-allow-methods"], "GET,HEAD,OPTIONS,DELETE,POST,PUT",
                "{path}"
            );
            assert_eq!(
                headers["access-control-allow-headers"], "content-type",
                "{path}"
            );
            assert_eq!(headers["access-control-max-age"], "86400", "{path}");
        }
        for (path, status, body, origin) in [
            (
                "/asset-bundle-registry/nope",
                StatusCode::NOT_FOUND,
                "Not found",
                Some("*"),
            ),
            ("/nope", StatusCode::IM_A_TEAPOT, "app fallback", None),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::get(path)
                        .header("origin", "https://example.org")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status, "{path}");
            assert_eq!(
                response
                    .headers()
                    .get("access-control-allow-origin")
                    .map(|value| value.to_str().unwrap()),
                origin,
                "{path}"
            );
            let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
            assert_eq!(bytes.as_ref(), body.as_bytes(), "{path}");
        }
        let _ = std::fs::remove_dir_all(&files);
    }
}

#[cfg(test)]
mod referral_settings_tests {
    use super::{referral_settings, social_analytics};
    use crate::config::Config;

    /// The boot path's provider setup with `ENV` set and nothing else: the
    /// node starts, without analytics.
    #[test]
    fn env_alone_boots_without_analytics() {
        let http = crate::http::Outbound::for_tests();
        for env in ["prd", "production", ""] {
            let config = Config::parse_env_for_tests([
                ("DATABASE_URL", "postgres:///catalyst"),
                ("ENV", env),
            ])
            .unwrap();
            assert!(social_analytics(&config, &http).unwrap().is_none(), "{env}");
        }
        let config = Config::parse_env_for_tests([
            ("DATABASE_URL", "postgres:///catalyst"),
            ("ENV", "prd"),
            ("ANALYTICS_CONTEXT", "fixture"),
            ("ANALYTICS_API_URL", "https://analytics.example.org"),
            ("ANALYTICS_API_TOKEN", "fixture"),
        ])
        .unwrap();
        assert!(social_analytics(&config, &http).unwrap().is_some());
    }

    /// `HTTP_TRUSTED_PROXY_CIDRS` through `referral_settings` into the real
    /// referral route behind a real socket: a configured proxy's `X-Real-IP`
    /// is the stored client IP, any other peer is stored as itself.
    #[tokio::test]
    async fn configured_proxies_reach_the_referral_client_ip_path() {
        use crate::social::communities;
        let Some(pool) = crate::db::scratch::pool(4).await else {
            return;
        };
        for (cidrs, expected) in [
            ("127.0.0.1/32", "198.51.100.7"),
            ("192.0.2.0/24", "127.0.0.1"),
        ] {
            let config = Config::parse_env_for_tests([
                ("DATABASE_URL", "postgres:///catalyst"),
                ("HTTP_TRUSTED_PROXY_CIDRS", cidrs),
            ])
            .unwrap();
            let dir =
                std::env::temp_dir().join(format!("catalyst-referral-ip-{}", uuid::Uuid::new_v4()));
            let storage =
                std::sync::Arc::new(crate::storage::Storage::from_config(&dir).await.unwrap());
            let state = std::sync::Arc::new(communities::State::new(
                pool.clone(),
                storage,
                communities::Settings {
                    referral: communities::referral::Settings {
                        suspicious_window_ms: 0.0,
                        ..referral_settings(&config)
                    },
                    ..communities::Settings::for_tests()
                },
                None,
                &crate::http::Outbound::for_tests(),
                communities::State::explore_for_tests(&pool),
            ));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!(
                "http://{}/v1/referral-progress",
                listener.local_addr().unwrap()
            );
            let app = communities::router(state);
            let server = tokio::spawn(async move {
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                )
                .await
                .unwrap()
            });
            let (referrer, invited) = (
                alloy::signers::local::PrivateKeySigner::random(),
                alloy::signers::local::PrivateKeySigner::random(),
            );
            let mut request = crate::http::Outbound::for_tests()
                .post(&endpoint)
                .header("x-real-ip", "198.51.100.7")
                .json(&serde_json::json!({ "referrer": format!("{:#x}", referrer.address()) }));
            let headers = crate::shared::signed_fetch::test_support::signed_headers_by(
                &invited,
                "post",
                "/v1/referral-progress",
                "{}",
                true,
                &chrono::Utc::now().timestamp_millis().to_string(),
            )
            .await;
            for (name, value) in &headers {
                request = request.header(name.as_str(), value.to_str().unwrap());
            }
            let response = request.send().await.unwrap();
            assert_eq!(
                response.status(),
                axum::http::StatusCode::NO_CONTENT,
                "{cidrs}"
            );
            let stored: String = sqlx::query_scalar(
                "SELECT invited_user_ip FROM social.referral_progress WHERE invited_user = $1",
            )
            .bind(format!("{:#x}", invited.address()))
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(stored, expected, "{cidrs}");
            server.abort();
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn referral_quotas_trust_exactly_the_configured_proxy_networks() {
        let parse = |vars: &[(&str, &str)]| {
            Config::parse_env_for_tests(
                [("DATABASE_URL", "postgres:///catalyst")]
                    .iter()
                    .chain(vars)
                    .copied(),
            )
            .unwrap()
        };
        assert!(
            referral_settings(&parse(&[]))
                .trusted_proxy_cidrs
                .is_empty(),
            "no proxy is trusted unless configured"
        );
        let config = parse(&[("HTTP_TRUSTED_PROXY_CIDRS", "10.0.0.0/8,2001:db8::/32")]);
        let trusted = referral_settings(&config).trusted_proxy_cidrs;
        assert_eq!(trusted, config.node.http_trusted_proxy_cidrs);
        assert_eq!(
            trusted.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["10.0.0.0/8", "2001:db8::/32"]
        );
    }
}
