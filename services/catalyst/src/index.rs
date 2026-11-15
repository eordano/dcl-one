//! Chain and marketplace indexing in a process without an HTTP listener.
use crate::settings::{jobs_from_config, land_settings, market_settings};
use crate::{config::Config, content, db, http, land_authz, shop};
use anyhow::Context;
use std::{sync::Arc, time::Duration};

pub async fn run(mut config: Config) -> anyhow::Result<()> {
    config.node.init_tracing();
    tracing::info!("{}", catalyst_config::posture::startup_line(&config));
    config.resolve_secret_files()?;
    let pool = db::connect_pool(
        &config.node.database_url,
        &db::PoolSettings {
            max_connections: config.node.pg_pool_size,
            ..Default::default()
        },
    )
    .await
    .context("connecting index worker to DATABASE_URL")?;
    db::bootstrap(&pool).await?;
    let http = http::Outbound::new(http::HttpClientCfg::serve(
        config.content.connections_max_idle,
    ))?;
    let jobs = jobs_from_config(&config, &pool);
    let market = Arc::new(shop::market::MarketplaceState::new(
        pool.clone(),
        http.clone(),
        market_settings(&config, &content::own_hosts(&config))?,
    ));
    market.spawn_index_jobs(&jobs);
    shop::rentals::index::spawn_job(&market, &jobs, config.shop.shop_rentals_index_enabled);
    let land = Arc::new(land_authz::LandAuthzState::new(
        pool.clone(),
        http,
        land_settings(&config)?,
        Some(market.index.clone()),
    ));
    land.spawn_jobs(&jobs);
    let shutdown = crate::serve::shutdown_signal();
    tracing::info!(jobs = ?jobs.report().iter().map(|j| &j.name).collect::<Vec<_>>(), "index worker started");
    shutdown.await;
    let aborted = jobs.shutdown(Duration::from_secs(30)).await;
    if !aborted.is_empty() {
        tracing::warn!(?aborted, "index jobs aborted at shutdown");
    }
    pool.close().await;
    Ok(())
}
