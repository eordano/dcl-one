//! Settings shared by the HTTP server and the indexing worker.
use crate::{config::Config, land_authz, shop};
use anyhow::Context;
use std::time::Duration;

pub(crate) fn market_settings(
    config: &Config,
    self_hosts: &[String],
) -> anyhow::Result<shop::market::Settings> {
    Ok(shop::market::Settings::from_config(
        config,
        &shop::market::Upstreams {
            eth_collections_subgraph_url: config
                .market_upstreams
                .eth_collections_subgraph_url
                .clone(),
            matic_collections_subgraph_url: config
                .market_upstreams
                .matic_collections_subgraph_url
                .clone(),
            marketplace_subgraph_url: config.market_upstreams.marketplace_subgraph_url.clone(),
            land_subgraph_url: config.market_upstreams.land_subgraph_url.clone(),
            trades_sync_upstream_url: config.market_upstreams.trades_sync_upstream_url.clone(),
            trades_sync_interval_secs: config.market_upstreams.trades_sync_interval_secs,
            trades_sync_pages_per_run: config.market_upstreams.trades_sync_pages_per_run,
            trades_serve_partial: config.market_upstreams.trades_serve_partial,
            profile_cdn_base_url: config.market_upstreams.profile_cdn_base_url.clone(),
            nft_worker_base_url: config.market_upstreams.nft_worker_base_url.clone(),
            explorer_escrow_grants_enabled: config.market_upstreams.explorer_escrow_grants_enabled,
            lambdas_catalog_extensions_enabled: config
                .market_upstreams
                .lambdas_catalog_extensions_enabled,
        },
    )?
    .with_market(
        shop::market::MarketSettings::parse(
            config.marketplace.marketplace_mirrors_enabled,
            config.marketplace.marketplace_mirror_interval_secs,
            config.marketplace.marketplace_mirror_pages_per_tick,
            config.marketplace.trade_rpc_urls.as_deref(),
            Config::non_empty(&config.marketplace.market_admin_token),
            config.marketplace.marketplace_catalog_cache_ttl_secs,
            &config.social.marketplace_base_url,
            self_hosts,
        )
        .context("MARKETPLACE_* / TRADE_RPC_URLS configuration")?
        .with_indexes(checked_indexes(shop::market::chain_walk::IndexSettings {
            nft_enabled: config.marketplace.marketplace_nft_index_enabled,
            mana_transfers_enabled: config.marketplace.marketplace_mana_transfers_index_enabled,
            trade_events_enabled: config.marketplace.marketplace_trade_events_index_enabled,
            sales_enabled: config.marketplace.marketplace_sales_index_enabled,
            interval: Duration::from_secs(
                config.marketplace.marketplace_index_interval_secs.max(1),
            ),
            requests_per_pass: config
                .marketplace
                .marketplace_index_requests_per_pass
                .max(1),
            window_blocks: config.marketplace.marketplace_index_window_blocks.max(1),
            address_chunk: config.marketplace.marketplace_index_address_chunk.max(1),
            start_block_ethereum: config.marketplace.marketplace_index_start_block_ethereum,
            start_block_matic: config.marketplace.marketplace_index_start_block_matic,
            stop_block_ethereum: config.marketplace.marketplace_index_stop_block_ethereum,
            stop_block_matic: config.marketplace.marketplace_index_stop_block_matic,
            neighbours_interval: Duration::from_secs(
                config.marketplace.marketplace_neighbours_interval_secs,
            ),
            coupon_refresh_interval: Duration::from_secs(
                config.marketplace.marketplace_coupon_refresh_interval_secs,
            ),
            chain_id_ethereum: config.marketplace.marketplace_index_chain_id_ethereum,
            chain_id_matic: config.marketplace.marketplace_index_chain_id_matic,
        })?)
        .with_tips(shop::market::tips::TipSettings::parse(
            &config.marketplace.marketplace_tip_min_amount_wei,
            config.marketplace.marketplace_tip_max_age_secs,
            config.marketplace.marketplace_tip_max_pending,
        )?)
        .with_trusted_proxies(config.node.http_trusted_proxy_cidrs.clone()),
    ))
}

fn checked_indexes(
    indexes: shop::market::chain_walk::IndexSettings,
) -> anyhow::Result<shop::market::chain_walk::IndexSettings> {
    indexes.check_chains().map_err(anyhow::Error::msg)?;
    Ok(indexes)
}

pub(crate) fn land_settings(config: &Config) -> anyhow::Result<land_authz::Settings> {
    let mut land_settings =
        land_authz::Settings::for_network(&config.node.eth_network, config.content.eth_rpc_url()?)
            .with_context(|| {
                format!(
                    "ETH_NETWORK={} names no LAND/Estate registries (mainnet|sepolia)",
                    config.node.eth_network
                )
            })?;
    land_settings.index_enabled = config.land_authz.land_authz_index_enabled;
    land_settings.interval =
        Duration::from_secs(config.land_authz.land_authz_index_interval_secs.max(30));
    land_settings.requests_per_pass = config.land_authz.land_authz_index_requests_per_pass.max(1);
    land_settings.replay_from_block = config.land_authz.land_authz_replay_from_block;
    land_settings.stop_block = config.land_authz.land_authz_stop_block;
    Ok(land_settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_land_settings_keep_limits_and_replay_bounds() {
        let mut config =
            Config::parse_env_for_tests([("DATABASE_URL", "postgres:///catalyst")]).unwrap();
        config.land_authz.land_authz_index_interval_secs = 0;
        config.land_authz.land_authz_index_requests_per_pass = 0;
        config.land_authz.land_authz_replay_from_block = Some(42);
        config.land_authz.land_authz_stop_block = Some(99);
        let settings = land_settings(&config).unwrap();
        assert_eq!(settings.interval, Duration::from_secs(30));
        assert_eq!(settings.requests_per_pass, 1);
        assert_eq!(settings.replay_from_block, Some(42));
        assert_eq!(settings.stop_block, Some(99));
    }

    #[test]
    fn shared_market_settings_reject_unsupported_chain_ids() {
        let mut config =
            Config::parse_env_for_tests([("DATABASE_URL", "postgres:///catalyst")]).unwrap();
        config.marketplace.marketplace_index_chain_id_ethereum = 999;
        let error = market_settings(&config, &[])
            .err()
            .expect("unsupported chain");
        assert!(
            error
                .to_string()
                .contains("MARKETPLACE_INDEX_CHAIN_ID_ETHEREUM=999")
        );
    }
}
