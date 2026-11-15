//! Root HTTP server states.
use super::settings::*;
use crate::social::{
    self, badges, communities, media, messaging, notifications, profile_images, rpc as social_rpc,
};
use crate::{
    comms, config::Config, content, explore, http, jobs, ops, price, shared, shop, storage, worlds,
};
use anyhow::Context;
use std::{sync::Arc, time::Duration};

/// What the operator-plane states read from the rest of `serve`.
pub(super) struct OpsSources<'a> {
    pub(super) market: &'a Arc<shop::market::MarketplaceState>,
    pub(super) price: &'a Arc<price::State>,
    pub(super) economy: &'a Arc<shop::evm::State>,
    pub(super) eip1654: Option<Arc<dyn crate::auth::eip1654::Eip1654Validator>>,
    pub(super) public_url: &'a str,
}

/// The operator-plane states, one per old unit, with their jobs attached.
pub(super) struct OpsStates {
    pub(super) credits: Arc<shop::credits::State>,
    pub(super) rpc: ops::rpc::Shared,
    pub(super) imposters: explore::imposters::Shared,
}

pub(super) async fn ops_states(
    config: &Config,
    pool: &sqlx::PgPool,
    http_client: &http::Outbound,
    jobs: &Arc<jobs::Jobs>,
    sources: OpsSources<'_>,
) -> anyhow::Result<OpsStates> {
    let non_empty = Config::non_empty;

    let credits = Arc::new(shop::credits::State::new(
        pool.clone(),
        http_client.clone(),
        credits_settings(config)?,
        shop::credits::Sources {
            market: Some(sources.market.clone()),
            price: Some(sources.price.clone()),
            economy: Some(sources.economy.clone()),
        },
        sources.eip1654.clone(),
    ));
    credits.spawn_jobs(jobs);
    shop::credits::goals::spawn_job(&credits, jobs);

    let rpc = ops::rpc::State::new(
        ops::rpc::Settings::parse(
            [
                ("mainnet", config.rpc.rpc_upstream_mainnet.clone()),
                ("ethereum", config.rpc.rpc_upstream_ethereum.clone()),
                ("sepolia", config.rpc.rpc_upstream_sepolia.clone()),
                ("polygon", config.rpc.rpc_upstream_polygon.clone()),
                ("matic", config.rpc.rpc_upstream_matic.clone()),
                ("amoy", config.rpc.rpc_upstream_amoy.clone()),
                ("mumbai", config.rpc.rpc_upstream_mumbai.clone()),
                ("arbitrum", config.rpc.rpc_upstream_arbitrum.clone()),
                ("optimism", config.rpc.rpc_upstream_optimism.clone()),
                ("avalanche", config.rpc.rpc_upstream_avalanche.clone()),
                ("binance", config.rpc.rpc_upstream_binance.clone()),
                ("fantom", config.rpc.rpc_upstream_fantom.clone()),
            ],
            non_empty(&config.rpc.catalyrst_rpc_admin_token),
            config.rpc.catalyrst_rpc_allow_private_upstream,
            &format!("{}/rpc", sources.public_url.trim_end_matches('/')),
        )
        .context("RPC_UPSTREAM_* / CATALYRST_RPC_* configuration")?,
        http_client.clone(),
        pool.clone(),
    );

    let imposters = explore::imposters::State::new(
        explore::imposters::Settings::resolve(explore::imposters::RawSettings {
            store_root: non_empty(&Some(config.imposters.bvimposters_store_root.clone())),
            store_max_bytes: Some(config.imposters.bvimposters_store_max_bytes),
            mem_cache_bytes: Some(config.imposters.bvimposters_mem_cache_bytes),
            bake: imposters_bake_settings(config),
        })
        .context("BVIMPOSTERS_* configuration")?,
    )
    .await
    .context("opening the imposter store")?;
    imposters.spawn_jobs(jobs);

    Ok(OpsStates {
        credits,
        rpc,
        imposters,
    })
}

/// The comms gatekeeper's settings, resolved once: the gatekeeper reads the
/// LiveKit credentials from them (it also removes the participants the
/// archipelago connector kicks), and the worlds settings take the LiveKit
/// client URL.
/// Worlds advertise their comms mint only against a real LiveKit: dev
/// credentials mint tokens no cluster accepts, and no signaling URL has no room.
pub(super) fn worlds_comms_online(livekit: &shared::livekit::LiveKit) -> bool {
    livekit.configured && !livekit.client_url.is_empty()
}

/// The upstream forwarder of one lane when `var` names an upstream (a public
/// http(s) base URL that is not this node), its job spawned; `None` when unset.
#[allow(clippy::too_many_arguments)]
pub(super) fn forwarder(
    var: &str,
    url: Option<&str>,
    lane: content::forward::Lane,
    config: &Config,
    pool: &sqlx::PgPool,
    storage: &Arc<storage::Storage>,
    self_hosts: &[String],
    jobs: &Arc<jobs::Jobs>,
) -> anyhow::Result<Option<Arc<content::forward::Forwarder>>> {
    let Some(url) = url.map(str::trim).filter(|u| !u.is_empty()) else {
        return Ok(None);
    };
    crate::shared::upstream::check_upstream(var, url, self_hosts)?;
    content::forward::check_target(var, url).map_err(anyhow::Error::msg)?;
    let forwarder = Arc::new(content::forward::Forwarder::new(
        pool.clone(),
        storage.clone(),
        content::forward::client().context("building the forwarding HTTP client")?,
        lane,
        url,
        content::forward::Retry::new(
            config.writes.forward_max_attempts,
            config.writes.forward_retry_base_secs,
        ),
    ));
    forwarder.spawn(jobs);
    tracing::info!(var, lane = lane.job, "forwarding local deploys upstream");
    Ok(Some(forwarder))
}

/// The asset-bundle converter `POST /reprocess-ab` hands world scenes to: the
/// abgen runtime at `ABGEN_ADMIN_URL`, which must be a loopback address (the
/// operator console's rule). Unset, the route answers 500.
pub(super) fn reprocess_converter(
    config: &Config,
    http_client: &http::Outbound,
) -> anyhow::Result<Option<worlds::core::reprocess::Converter>> {
    let Some(url) = Config::non_empty(&config.admin.abgen_admin_url) else {
        return Ok(None);
    };
    let abgen =
        ops::admin::console::Abgen::parse(&url, None, http_client).context("ABGEN_ADMIN_URL")?;
    Ok(Some(worlds::core::reprocess::Converter::new(
        abgen.base(),
        http_client.raw().clone(),
    )))
}

/// The stage-5 worlds states: the product surface (publish, permissions,
/// settings, presence, operator routes), the world/player/env storage, the
/// authoritative scene host (behind `SCENE_HOST_ENABLED`) and the worlds
/// federation mirror, every one over the ONE pool, blob store, client,
/// permission core and job lifecycle; `worlds::app_router` owns their
/// routes. `catalyst pulse` is the realtime process beside this one; they
/// meet on `NATS_URL` and the room authority comms mounts.
#[allow(clippy::too_many_arguments)]
pub(super) async fn worlds_states(
    config: &Config,
    pool: &sqlx::PgPool,
    storage: &Arc<storage::Storage>,
    http_client: &http::Outbound,
    policy: &Arc<worlds::policy::Policy>,
    eip1654: Option<Arc<dyn crate::auth::eip1654::Eip1654Validator>>,
    local_urls: &[String],
    jobs: &Arc<jobs::Jobs>,
    presence: Arc<comms::presence::Presence>,
) -> anyhow::Result<worlds::Stage5> {
    let max_concurrent_uploads = worlds::core::partial::uploads_within_pool(
        config.worlds.max_concurrent_uploads,
        config.node.pg_pool_size,
    );
    if max_concurrent_uploads < config.worlds.max_concurrent_uploads {
        tracing::warn!(
            configured = config.worlds.max_concurrent_uploads,
            effective = max_concurrent_uploads,
            pg_pool_size = config.node.pg_pool_size,
            "MAX_CONCURRENT_UPLOADS lowered to three quarters of PG_POOL_SIZE"
        );
    }
    let non_empty = Config::non_empty;

    let core = Arc::new(
        worlds::core::CoreState::new(
            pool.clone(),
            storage.clone(),
            policy.clone(),
            worlds::core::Settings::resolve(
                non_empty(&config.worlds.catalyrst_worlds_admin_token),
                worlds::core::UploadLimits {
                    max_in_flight_upload_bytes: config.worlds.max_in_flight_upload_bytes,
                    max_concurrent_uploads,
                    max_in_flight_upload_files: config.worlds.max_in_flight_upload_files,
                    multipart_upload_timeout_ms: config.worlds.multipart_upload_timeout_ms,
                    deployment_processing_timeout_ms: config
                        .worlds
                        .deployment_processing_timeout_ms,
                    max_concurrent_uploads_per_source: u64::from(
                        config.content.max_concurrent_uploads_per_source,
                    ),
                    max_in_flight_upload_bytes_per_source: config
                        .content
                        .max_in_flight_upload_bytes_per_source
                        .unwrap_or(worlds::core::upload_limits::DEFAULT_MAX_IN_FLIGHT_UPLOAD_BYTES_PER_SOURCE),
                },
            )
            .and_then(|s| {
                s.with_global_scenes_urn(config.worlds_content.global_scenes_urn.as_deref())
                    .with_scene_eviction_ttl_ms(config.worlds.scene_eviction_ttl_ms)
                    .with_public_url(&worlds::Settings::from_config(config).public_url)
                    .with_max_file_count(config.worlds.max_file_count)
                    .with_access_caps(
                        config.worlds.access_max_wallets,
                        config.worlds.access_max_communities,
                    )
                    .with_owner_reconcile_interval_secs(
                        config.worlds.worlds_owner_reconcile_interval_secs,
                    )
                    // The variables content's partial uploads share; each domain keeps its own budget.
                    .with_partial_uploads(worlds::core::PartialUploadLimits {
                        pending_deployment_ttl_ms: config.content.pending_deployment_ttl_ms,
                        cleanup_interval_ms: config.worlds.partial_upload_cleanup_interval_ms,
                        max_pending_per_deployer: u64::from(
                            config.content.max_pending_deployments_per_deployer,
                        ),
                        max_pending_per_world: config.worlds.max_pending_deployments_per_world,
                        pending_bytes_per_world_factor: config
                            .worlds
                            .pending_bytes_per_world_factor,
                        max_pending_bytes_per_deployer: config
                            .content
                            .max_pending_bytes_per_deployer,
                        max_pending_bytes_per_source: config.worlds.max_pending_bytes_per_source,
                        max_pending_bytes: config.content.max_pending_bytes,
                        max_bytes_per_minute: config.content.max_partial_upload_bytes_per_minute,
                        completed_upload_ttl_ms: config.worlds.completed_upload_ttl,
                    })
            })
            .context(
                "MAX_IN_FLIGHT_UPLOAD_* / MAX_CONCURRENT_UPLOADS / *_TIMEOUT_MS configuration",
            )?,
            eip1654.clone(),
        )
        .with_presence(presence)
        .with_allowance(worlds::core::holdings::configured(
            http_client.clone(),
            config.content.eth_rpc_url()?.as_deref(),
            non_empty(&config.economy.rpc_endpoint_polygon).as_deref(),
            &config.node.eth_network,
        ))
        // World notices persist through social's directory, as comms' do.
        .with_notices(worlds::core::notices::Notices::new(
            Some(Arc::new(social::comms_directory::Directory::new(
                pool.clone(),
            ))),
            &config.worlds.builder_url,
        ))
        .with_converter(reprocess_converter(config, http_client)?)
        .with_forwarder(forwarder(
            "WORLDS_FORWARD_UPSTREAM_URL",
            config.worlds.worlds_forward_upstream_url.as_deref(),
            worlds::core::forwarding::LANE,
            config,
            pool,
            storage,
            &content::own_hosts(config),
            jobs,
        )?),
    );
    core.spawn_jobs(jobs);

    let world_storage = Arc::new(worlds::storage::StorageState::new(
        pool.clone(),
        policy.clone(),
        worlds::storage::Settings::resolve(worlds::storage::RawSettings {
            encryption_key: non_empty(&config.worlds.encryption_key),
            authoritative_server_address: non_empty(&config.social.authoritative_server_address),
            authorized_addresses: config.worlds.authorized_addresses.clone(),
            storage_cache_enabled: config.worlds.storage_cache_enabled.clone(),
            storage_cache_ttl_seconds: config.worlds.storage_cache_ttl_seconds,
            storage_cache_max: config.worlds.storage_cache_max,
            storage_cache_max_value_bytes: config.worlds.storage_cache_max_value_bytes,
            env_max_value_size_bytes: config.worlds.env_storage_max_value_size_bytes,
            env_max_total_size_bytes: config.worlds.env_storage_max_total_size_bytes,
            world_max_value_size_bytes: config.worlds.world_storage_max_value_size_bytes,
            world_max_total_size_bytes: config.worlds.world_storage_max_total_size_bytes,
            player_max_value_size_bytes: config.worlds.player_storage_max_value_size_bytes,
            player_max_total_size_bytes: config.worlds.player_storage_max_total_size_bytes,
            export_max_concurrent_per_scene: config.worlds.export_max_concurrent_per_scene,
            export_max_concurrent_per_pod: config.worlds.export_max_concurrent_per_pod,
            export_scene_cooldown_seconds: config.worlds.export_scene_cooldown_seconds,
            export_timeout_seconds: config.worlds.export_timeout_seconds,
            export_max_size_bytes: config.worlds.export_max_size_bytes,
            export_stream_query_timeout_ms: config.worlds.pg_component_stream_query_timeout,
        })
        .context("ENCRYPTION_KEY / AUTHORIZED_ADDRESSES / *_STORAGE_MAX_* configuration")?,
        eip1654.clone(),
    ));

    let scene_host = if config.worlds.scene_host_enabled {
        let settings = worlds::scene_state::Settings::resolve(
            worlds::scene_state::RawSettings {
                local_scene_path: non_empty(&config.worlds.scene_host_local_scene_path),
                debugging_secret: non_empty(&config.worlds.scene_host_debugging_secret),
                admin_token: non_empty(&config.worlds.scene_host_admin_token),
                auth_timeout_secs: config.worlds.scene_host_auth_timeout_secs,
                disable_js_runtime: config.worlds.scene_host_disable_js_runtime,
                realm_name: config.node.realm_name.clone(),
                commit_hash: config.node.commit_hash.clone(),
                js_heap_limit_mb: config.worlds.js_heap_limit_mb,
                js_tick_budget_ms: config.worlds.js_tick_budget_ms,
                js_shutdown_join_ms: config.worlds.js_shutdown_join_ms,
                js_update_failure_cap: config.worlds.js_update_failure_cap,
                client_outbound_max: config.worlds.client_outbound_max,
                client_inbound_max: config.worlds.client_inbound_max,
                crdt_max_components: config.worlds.crdt_max_components,
                ws_max_frame_bytes: config.worlds.ws_max_frame_bytes,
                storage_url: non_empty(&config.worlds.scene_host_storage_url),
                storage_allow_http: config.worlds.scene_host_storage_allow_http,
                authoritative_key: non_empty(&config.worlds.scene_host_authoritative_key),
                delegation_ttl_secs: config.worlds.scene_host_delegation_ttl_secs,
                delegation_minter_url: non_empty(&config.worlds.scene_host_delegation_minter_url),
                delegation_minter_token: non_empty(
                    &config.worlds.scene_host_delegation_minter_token,
                ),
                storage_delegation: non_empty(&config.worlds.scene_host_storage_delegation),
                signed_fetch_max_response_bytes: config.worlds.signed_fetch_max_response_bytes,
                signed_fetch_max_body_bytes: config.worlds.signed_fetch_max_body_bytes,
                signed_fetch_max_in_flight: config.worlds.signed_fetch_max_in_flight,
                signed_fetch_timeout_ms: config.worlds.signed_fetch_timeout_ms,
                max_scenes: config.worlds.scene_host_max_scenes,
                autoload: config.worlds.scene_host_autoload,
                sweep_interval_secs: config.worlds.scene_host_sweep_interval_secs,
                idle_unload_secs: config.worlds.scene_host_idle_unload_secs,
            },
            http_client,
        )
        .context("SCENE_HOST_* configuration")?;
        let host = Arc::new(
            worlds::scene_state::SceneHostState::new(
                pool.clone(),
                storage.clone(),
                policy.clone(),
                settings,
                eip1654.clone(),
            )
            .context("building the scene host")?
            .with_secret_max_attempts(config.worlds.shared_secret_max_attempts_per_minute),
        );
        host.spawn_jobs(jobs);
        Some(host)
    } else {
        tracing::info!("SCENE_HOST_ENABLED=false: no authoritative scene host");
        None
    };

    let fed_cfg = worlds::fed::WorldsFedConfig::new(
        config.worlds_fed.worlds_fed_peers_file.clone(),
        config.worlds_fed.worlds_fed_poll_interval_secs,
        config.worlds_fed.worlds_fed_max_response_bytes,
        config.worlds_fed.worlds_fed_max_worlds_per_peer,
        config.worlds_fed.worlds_fed_allow_insecure_loopback_peers,
    )
    .context("WORLDS_FED_* configuration")?
    .with_own_urls(local_urls.to_vec());
    let fed = worlds::fed::FedState::boot(
        pool.clone(),
        fed_cfg,
        non_empty(&config.worlds.catalyrst_worlds_admin_token),
    )
    .await
    .context("worlds federation")?;
    fed.spawn_jobs(jobs);

    tracing::info!(
        scene_host = scene_host.is_some(),
        worlds_admin = config.worlds.catalyrst_worlds_admin_token.is_some(),
        nats = config.worlds.nats_url.is_some(),
        federation = config.worlds_fed.worlds_fed_peers_file.is_some(),
        "worlds routes"
    );
    Ok(worlds::Stage5 {
        core,
        storage: world_storage,
        scene_host,
        fed,
        writes_enabled: config.worlds.worlds_writes_enabled,
    })
}

/// The comms states: the gatekeeper over the ONE pool and client,
/// the worlds permission core and room resolver
/// (`comms::interfaces::{AccessPolicy, SceneIdentity}`), the live presence its
/// webhook feeds (shared with worlds) and the worlds core as its room-event
/// sink (the access log), owning `serve`'s one NATS link (`NATS_URL`); and
/// archipelago's ws-connector, built from it, over the same pool and client,
/// the worlds permission core as its ban authority
/// (`comms::interfaces::AccessPolicy`) and the gatekeeper's link and LiveKit
/// (the connector holds no LiveKit credential); on the `primary`, the one
/// process that holds the connector's sockets, the gatekeeper's island
/// subscriber then delivers to them by call.
/// `serve` mounts archipelago's routes
/// right after `worlds::app_router`, where that router used to add them, and
/// the gatekeeper's right before the social router, where it held them.
pub(super) struct CommsStage {
    pub(super) archipelago: crate::comms::archipelago::AppState,
    pub(super) gatekeeper: crate::comms::gatekeeper::SharedState,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn comms_states(
    config: &Config,
    pool: &sqlx::PgPool,
    http_client: &http::Outbound,
    market: &Arc<shop::market::MarketplaceState>,
    comms_settings: comms::gatekeeper::Settings,
    policy: &Arc<worlds::policy::Policy>,
    ban_status: Arc<dyn comms::gatekeeper::moderation::BanStatus>,
    room_events: Arc<dyn comms::gatekeeper::events::RoomEventSink>,
    room_event_store: Option<Arc<dyn comms::gatekeeper::events::RoomEventStore>>,
    presence: Arc<comms::presence::Presence>,
    eip1654: Option<Arc<dyn crate::auth::eip1654::Eip1654Validator>>,
    primary: bool,
    jobs: &Arc<jobs::Jobs>,
) -> anyhow::Result<CommsStage> {
    use crate::comms::archipelago;
    use crate::comms::interfaces::AccessPolicy;

    let non_empty = Config::non_empty;
    let archipelago_settings = archipelago::Settings::resolve(archipelago::config::RawSettings {
        max_sockets: config.worlds.ws_max_sockets,
        max_sockets_per_ip: config.worlds.ws_max_sockets_per_ip,
        trusted_proxy_cidrs: config.node.http_trusted_proxy_cidrs.clone(),
        handshake_timeout_ms: config.worlds.handshake_timeout,
        deny_list_url: config.worlds.deny_list(),
        island_changed_dedup_ms: config.worlds.island_changed_dedup_ms,
        commit_hash: config.node.commit_hash.clone(),
    });

    let social_directory = Arc::new(social::comms_directory::Directory::new(pool.clone()));
    let gatekeeper = comms::gatekeeper::CommsState::new(
        pool.clone(),
        http_client.clone(),
        comms_settings,
        Some(market.index.clone()),
        eip1654,
        policy.clone() as Arc<dyn AccessPolicy>,
        Arc::new(worlds::rooms::SceneResolver::new(pool.clone())),
        presence,
        social_directory,
    )
    .with_ban_status(ban_status.clone())
    .with_name_owner_reader(market.clone())
    .with_deny_list_url(config.worlds.deny_list())
    .with_cluster(comms::gatekeeper::admission::cluster::ClusterConfig {
        nats_url: non_empty(&config.worlds.nats_url),
        takeover_retry_delay_ms: config.worlds.cluster_takeover_retry_delay_ms,
        island_token_ttl_seconds: config.worlds.cluster_island_token_ttl_seconds,
        self_hosted_token_quarantine: config.worlds.cluster_self_hosted_token_quarantine,
        drain_timeout_ms: config.worlds.cluster_drain_timeout_ms,
        peer_state_max: config.worlds.cluster_peer_state_max,
        peer_state_ttl_ms: config.worlds.cluster_peer_state_ttl_ms,
        control_v4_audience: non_empty(&config.worlds.archipelago_control_v4_audience),
    })
    .context("ARCHIPELAGO_CONTROL_V4_AUDIENCE and NATS_URL configuration")?
    .with_max_users_per_world(config.worlds.max_users_per_world)
    .with_shared_secret_max_attempts(config.worlds.shared_secret_max_attempts_per_minute)
    .with_room_authority_key(non_empty(&config.room_authority.pulse_room_authority_key).as_deref())?
    .with_room_event_sink(room_events);
    let gatekeeper = match room_event_store {
        Some(store) => gatekeeper.with_room_event_store(store),
        None => gatekeeper,
    };
    let gatekeeper = Arc::new(gatekeeper);
    gatekeeper.spawn_presence_recovery(jobs);

    let archipelago = archipelago::build_state(archipelago_settings, gatekeeper.clone(), jobs)
        .await
        .context("wiring archipelago")?;
    if primary {
        gatekeeper.start_island_feed(jobs, archipelago.clone());
    }
    archipelago::spawn_jobs(&archipelago, jobs);
    Ok(CommsStage {
        archipelago,
        gatekeeper,
    })
}

/// Social's community exits, enforced by the worlds core on a spawned task
/// (the social write has committed; the re-decision never delays it).
pub(super) struct WorldsCommunityAccess(pub(super) worlds::core::SharedState);

impl communities::WorldAccessRevocation for WorldsCommunityAccess {
    fn member_removed(
        &self,
        community_id: crate::shared::identity::CommunityId,
        member: &crate::shared::identity::Address,
    ) {
        let (core, member) = (self.0.clone(), member.clone());
        tokio::spawn(async move {
            core.community_member_removed(community_id.as_uuid(), &member)
                .await
        });
    }

    fn community_closed(&self, community_id: crate::shared::identity::CommunityId) {
        let core = self.0.clone();
        tokio::spawn(async move { core.community_closed(community_id.as_uuid()).await });
    }
}

/// Social's boot-time provider setup: the bootstrap check, then the analytics
/// adapter, which is configured by its own settings only.
pub(super) fn social_analytics(
    config: &Config,
    http_client: &http::Outbound,
) -> anyhow::Result<Option<social::analytics::Analytics>> {
    config
        .social
        .validate_provider_bootstrap()
        .map_err(anyhow::Error::msg)?;
    if config.social.community_compliance_api_key.is_none() {
        tracing::info!(
            "OPEN_AI_API_KEY unset: community AI compliance runs without its provider and skips classification"
        );
    }
    social::analytics::Analytics::new(
        http_client,
        social::analytics::Settings {
            context: config.social.social_analytics_context.clone(),
            api_url: config.social.social_analytics_api_url.clone(),
            api_token: config.social.social_analytics_api_token.clone(),
            environment: config.social.social_environment.clone(),
            request_timeout_ms: config.social.social_analytics_request_timeout_ms,
        },
    )
    .map_err(anyhow::Error::msg)
}

/// The social bundle's states (the old catalyrst-social members: communities,
/// notifications, badges, media, plus profile-images, the social RPC and the
/// MLS delivery service), every one over the same pool, client, blob store
/// and validator; `social::router` owns their routes, `serve` mounts the MLS
/// routes under `/comms-gatekeeper`. The gatekeeper's state (built in
/// `comms_states`) is also the social socket's voice runtime
/// (`comms::gatekeeper::voice::rooms`), the local call that replaced the old
/// `COMMS_GATEKEEPER_URL` hop.
#[allow(clippy::too_many_arguments)]
pub(super) fn social_states(
    config: &Config,
    pool: &sqlx::PgPool,
    storage: &Arc<storage::Storage>,
    http_client: &http::Outbound,
    content: &Arc<content::ContentState>,
    market: &Arc<shop::market::MarketplaceState>,
    comms: comms::gatekeeper::SharedState,
    eip1654: Option<Arc<dyn crate::auth::eip1654::Eip1654Validator>>,
    explore: &explore::SharedState,
    onboarding: Arc<explore::api::onboarding::OnboardingState>,
    metrics_handle: comms::metrics::PrometheusHandle,
) -> anyhow::Result<social::States> {
    config
        .social
        .validate_provider_bootstrap()
        .map_err(anyhow::Error::msg)?;
    let non_empty = Config::non_empty;
    let public_url = config
        .node
        .public_url
        .clone()
        .unwrap_or_else(|| format!("http://{}", config.node.bind_addr()));
    let content_public_url = content.settings.content_public_url.clone();
    let profile_images_settings = profile_images::Settings::parse(profile_images::RawSettings {
        cache_dir: config.social.profile_images_cache_dir.clone(),
        cache_ttl_seconds: config.social.profile_images_cache_ttl_seconds,
        cache_max_bytes: config.social.profile_images_cache_max_bytes,
        origin_url: config.social.profile_images_origin_url.clone(),
        render_enabled: config.social.profile_images_render_enabled,
        renderer: config.social.profile_images_renderer.clone(),
        avatar_render_bin: config.social.profile_images_avatar_render_bin.clone(),
        godot_bin: config.social.profile_images_godot_bin.clone(),
        godot_project: config.social.profile_images_godot_project.clone(),
        rendering_method: config.social.profile_images_rendering_method.clone(),
        rendering_driver: config.social.profile_images_rendering_driver.clone(),
        dclenv: config.social.profile_images_dclenv.clone(),
        godot_headless: config.social.profile_images_godot_headless,
        godot_display: config.social.profile_images_godot_display.clone(),
        godot_extra_args: config.social.profile_images_godot_extra_args.clone(),
        render_timeout_seconds: config.social.profile_images_render_timeout_seconds,
        render_max_concurrent: config.social.profile_images_render_max_concurrent,
        render_workdir: config.social.profile_images_render_workdir.clone(),
        content_public_url: content_public_url.clone(),
    })
    .context("PROFILE_IMAGES_* configuration")?;
    let faces = communities::profiles::FaceUrls::new(
        market.settings.profile_cdn_base_url.as_deref(),
        &market.settings.profile_images_public_url,
        profile_images_settings.derives_faces(),
    );

    let communities = Arc::new(communities::State::new(
        pool.clone(),
        storage.clone(),
        communities::Settings {
            http_max_request_body_bytes: config.social.social_http_max_request_body_bytes,
            compliance: communities::compliance::Settings {
                environment: config.social.social_environment.clone().unwrap_or_default(),
                model: config.social.community_compliance_model.clone(),
                api_key: config.social.community_compliance_api_key.clone(),
                base_url: config.social.community_compliance_base_url.clone(),
                organization: config.social.community_compliance_organization.clone(),
                project: config.social.community_compliance_project.clone(),
            },
            admin_token: non_empty(&config.social.communities_admin_token),
            global_moderators: communities::Settings::global_moderators_from(
                &config.social.communities_global_moderators,
            ),
            features: communities::features::Settings {
                base_url: config.social.social_feature_flags_url.clone(),
                audience: config.social.social_service_base_url.clone(),
                interval: Duration::from_millis(config.social.social_feature_flags_refresh_ms),
                timeout: Duration::from_millis(config.social.social_feature_flags_timeout_ms),
                overrides: [
                    (
                        communities::features::AI_COMPLIANCE,
                        &config.social.social_ai_compliance_override,
                    ),
                    (
                        communities::features::DEV_AI_COMPLIANCE,
                        &config.social.social_dev_ai_compliance_override,
                    ),
                    (
                        communities::features::GLOBAL_MODERATORS,
                        &config.social.social_global_moderators_override,
                    ),
                    (
                        communities::features::REFERRAL_REGISTRATION_DISABLED,
                        &config.social.social_referral_registration_disabled_override,
                    ),
                ]
                .into_iter()
                .filter_map(|(key, value)| value.clone().map(|value| (key.into(), value)))
                .collect(),
            },
            restricted_names: communities::Settings::restricted_names_from(
                &config.social.restricted_names,
            ),
            cdn_url: config
                .social
                .communities_cdn_url
                .clone()
                .unwrap_or(public_url.clone())
                .trim_end_matches('/')
                .to_string(),
            faces: faces.clone(),
            archipelago_stats_url: non_empty(&config.social.archipelago_stats_url),
            places_ownership_check: config.social.communities_places_ownership_check,
            mirror_interval: Duration::from_secs(
                config.social.communities_mirror_interval_secs.max(1),
            ),
            referral: referral_settings(config),
        },
        eip1654.clone(),
        http_client,
        explore.clone(),
    ));

    let analytics = social_analytics(config, http_client)?;
    if let Some(analytics) = &analytics {
        communities.set_analytics(analytics.clone());
    }

    let notifications = Arc::new(notifications::State::new(
        pool.clone(),
        notifications::Settings {
            admin_token: non_empty(&config.social.catalyrst_notifications_admin_token),
            events_base_url: public_url.clone(),
            events_asset_base_url: explore.settings.events_asset_base_url.clone(),
            signing_key: non_empty(&config.social.signing_key),
            service_base_url: config.social.notifications_service_base_url.clone(),
            community_cdn_url: communities.settings.cdn_url.clone(),
            jump_in_site_url: explore.settings.jump_in_site_url.clone(),
            event_slack_notices: explore.settings.events_upcoming_summaries_enabled
                && explore.settings.operator_writes_enabled
                && explore
                    .settings
                    .events_slack_webhook
                    .as_deref()
                    .is_some_and(|endpoint| !endpoint.trim().is_empty()),
            email: notifications::EmailConfig {
                sendgrid_api_key: non_empty(&config.social.sendgrid_api_key),
                from_email: non_empty(&config.social.sendgrid_from_email),
                notification_template_id: non_empty(
                    &config.social.sendgrid_notification_template_id,
                ),
                validate_email_template_id: non_empty(
                    &config.social.sendgrid_validate_email_template_id,
                ),
                validate_credits_email_template_id: non_empty(
                    &config.social.sendgrid_validate_credits_email_template_id,
                ),
                account_base_url: config.social.account_base_url.clone(),
                turnstile_secret_key: non_empty(&config.social.turnstile_secret_key),
                domain_blacklist: notifications::EmailConfig::parse_domain_blacklist(
                    &config.social.email_domain_blacklist,
                ),
                sandbox_mode: config.social.sendgrid_sandbox_mode,
                environment: non_empty(&config.social.social_environment).unwrap_or_default(),
            },
            notification_service_token: non_empty(&config.social.notification_service_token),
            governance_token: non_empty(&config.social.notifications_governance_token),
            rewards_token: non_empty(&config.social.notifications_rewards_token),
            first_wear_enabled: config.social.first_wear_enabled,
            first_wear: notifications::FirstWearSettings {
                shop_item_base_url: config.social.shop_item_base_url.clone(),
                image_base: non_empty(&config.social.first_wear_image_base),
            },
            delivery_interval: Duration::from_secs(
                config.social.notification_delivery_interval_secs,
            ),
            broadcast_horizon: Duration::from_secs(
                config
                    .social
                    .notifications_broadcast_horizon_days
                    .max(1)
                    .saturating_mul(86_400),
            ),
            delivery_max_age: notifications::delivery::max_age_from_secs(
                config.social.notification_delivery_max_age_secs,
            ),
            current_version: config.social.social_rpc_current_version.clone(),
            commit_hash: config.node.commit_hash.clone(),
        },
        http_client.clone(),
        eip1654.clone(),
        Some(market.index.clone()),
    ));

    if let Some(analytics) = analytics {
        notifications.set_analytics(analytics);
    }
    communities.referral.set_mailer(notifications.email.clone());

    let badges = Arc::new(badges::State::new(
        pool.clone(),
        badges::Settings {
            admin_token: non_empty(&config.social.catalyrst_badges_admin_token),
            public_asset_base_url: config.social.badges_public_asset_base_url.clone(),
            assets_dir: non_empty(&config.social.badges_assets_dir).map(std::path::PathBuf::from),
            commit_hash: config.commit_hash(),
            processor: badges::jobs::ProcessorSettings {
                enabled: config.social.badges_processor_enabled,
                batch_size: i64::from(config.social.badges_processor_batch_size),
                max_event_age: (config.social.badges_event_max_age_secs > 0)
                    .then(|| Duration::from_secs(config.social.badges_event_max_age_secs)),
                deployment_max_lag: (config.social.badges_deployment_max_lag_secs > 0)
                    .then(|| Duration::from_secs(config.social.badges_deployment_max_lag_secs)),
                items_mirror_enabled: config.marketplace.marketplace_mirrors_enabled,
                ..badges::jobs::ProcessorSettings::default()
            },
        },
    ));

    let segment_webhooks = Arc::new(social::segment_webhooks::State::new(
        pool.clone(),
        social::segment_webhooks::Settings {
            signing_key: non_empty(&config.social.segment_signing_key),
            force_context_validation: config.social.force_explorer_context_validation,
            expected_realm: non_empty(&config.social.segment_expected_realm),
            expected_realm_url: non_empty(&config.social.segment_expected_realm_url),
            web_project_id: config.social.segment_web_project_id.clone(),
            retention: Duration::from_secs(
                u64::from(config.social.client_events_retention_days.max(1)) * 24 * 3600,
            ),
        },
        onboarding,
    ));

    let media = Arc::new(
        media::MediaState::new(
            pool.clone(),
            http_client.clone(),
            media::Settings::parse(media::RawSettings {
                backend: config.social.translate_backend.clone(),
                backend_url: config.social.translate_backend_url.clone(),
                backend_api_key: Config::non_empty(&config.social.translate_backend_api_key)
                    .unwrap_or_default(),
                llm_base_url: config.social.translate_llm_base_url.clone(),
                llm_api_key: Config::non_empty(&config.social.translate_llm_api_key)
                    .unwrap_or_default(),
                llm_model: config.social.translate_llm_model.clone(),
                char_limit: config.social.translate_char_limit,
                batch_limit: config.social.translate_batch_limit,
                request_timeout_secs: config.social.translate_request_timeout_secs,
                cache_ttl_days: config.social.translate_cache_ttl_days,
            })
            .context("TRANSLATE_* configuration")?,
        )
        .with_convert_sources(media::ConvertSources {
            remote: config.social.media_convert_remote_sources,
            own_url: Some(
                url::Url::parse(&config.node.own_url()).context("PUBLIC_URL is not a URL")?,
            ),
            storage: Some(storage.clone()),
            read_through: content.read_through().cloned(),
        }),
    );

    let profile_images = Arc::new(
        profile_images::ProfileImagesState::new(
            pool.clone(),
            storage.clone(),
            http_client.clone(),
            profile_images_settings,
        )
        .with_read_through(content.read_through().cloned()),
    );

    let messaging = Arc::new(messaging::MessagingState::new(
        pool.clone(),
        storage.clone(),
        eip1654.clone(),
        messaging::Settings {
            fed_peer_id: non_empty(&config.social.fed_peer_id),
            gatekeeper_auth_token: non_empty(&config.social.comms_gatekeeper_auth_token),
        },
    ));

    let ordinary_transport = shared::rpc::ws::OrdinaryQueueConfig {
        max_queue_size: config.social.ws_transport_max_queue_size,
        min_queue_size: config.social.ws_transport_min_queue_size,
        max_queue_size_limit: config.social.ws_transport_max_queue_size_limit,
        estimated_message_size: config.social.ws_transport_estimated_message_size,
        retry_delay_ms: config.social.ws_transport_retry_delay_ms,
        max_retry_attempts: config.social.ws_transport_max_retry_attempts,
        max_backoff_delay_ms: config.social.ws_transport_max_backoff_delay_ms,
    };
    ordinary_transport
        .capacity(config.social.ws_max_backpressure)
        .map_err(|error| anyhow::anyhow!(error))?;
    let rpc = Arc::new(social_rpc::State::new(
        pool.clone(),
        social_rpc::Settings {
            auth_window_secs: config.social.social_rpc_auth_window_secs,
            ws_auth_timeout_ms: config.social.ws_auth_timeout_in_ms,
            ws_auth_signature_expiration_ms: config.social.ws_auth_signature_expiration_in_ms,
            ws_idle_timeout_secs: config.social.ws_idle_timeout_in_seconds,
            private_voice_chat_expiration_ms: config.social.private_voice_chat_expiration_ms,
            private_voice_chat_job_interval_ms: config.social.private_voice_chat_job_interval_ms,
            private_voice_chat_expiration_batch_size: config
                .social
                .private_voice_chat_expiration_batch_size,
            community_voice_chat_polling_job_interval_ms: config
                .social
                .community_voice_chat_polling_job_interval_ms,
            ws_max_concurrent_connections: config.social.ws_max_concurrent_connections,
            ws_max_payload_bytes: config.social.ws_max_payload_length,
            ws_max_backpressure_bytes: config.social.ws_max_backpressure,
            ordinary_transport,
            friendship_limits: social_rpc::limits::FriendshipLimits {
                window_seconds: config.social.friendship_rate_limit_window_seconds,
                per_actor: config.social.friendship_rate_limit_per_actor,
                per_pair: config.social.friendship_rate_limit_per_pair,
            },
            admission: social_rpc::limits::AdmissionLimits {
                max_per_address: config.social.ws_max_connections_per_address as usize,
                subscriptions_per_second: config.social.ws_subscriptions_per_second,
                subscription_burst: config.social.ws_subscription_burst as usize,
                idle_budget_capacity: config.social.ws_address_budget_capacity as usize,
            },
            stream: social_rpc::stream::StreamLimits {
                max_payload_bytes: config.social.stream_ws_max_payload_length as usize,
                messages_per_second: config.social.stream_ws_messages_per_second,
                message_burst: config.social.stream_ws_message_burst as usize,
            },
            admin_token: non_empty(&config.social.catalyrst_social_rpc_admin_token),
            writes_enabled: config.writes.social_writes_enabled,
            commit_hash: config.node.commit_hash.clone(),
            current_version: config.social.social_rpc_current_version.clone(),
            metrics_path: config.social.wkc_metrics_public_path.clone(),
            metrics_bearer_token: config.social.wkc_metrics_bearer_token.clone(),
            metrics_reset_at_night: config.social.wkc_metrics_reset_at_night.as_deref()
                == Some("true"),
            metrics_handle: Some(metrics_handle),
            profile_source: Some(communities.profiles.registry()),
        },
        eip1654,
        comms.clone(),
    ));
    // Membership changes and deletions reach the open RPC streams; a voice
    // start reads the community's places and thumbnail.
    communities.set_updates(rpc.ctx.pubsub().clone());
    rpc.ctx.set_communities(communities.clone());

    tracing::info!(
        translate = media.backend_label(),
        profile_images = %profile_images.sources_label(),
        livekit = comms.livekit.configured,
        segment_webhooks = segment_webhooks.settings.signing_key.is_some(),
        "social routes"
    );
    Ok(social::States {
        communities,
        comms,
        messaging,
        notifications,
        badges,
        media,
        profile_images,
        rpc,
        segment_webhooks,
    })
}
