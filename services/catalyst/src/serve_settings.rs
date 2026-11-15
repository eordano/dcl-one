//! Root HTTP server settings.
use crate::social::communities;
use crate::{comms, config::Config, explore, shared, shop};
use anyhow::Context;
use std::time::Duration;

pub(super) fn imposters_bake_settings(config: &Config) -> explore::imposters::RawBakeSettings {
    let non_empty = Config::non_empty;
    explore::imposters::RawBakeSettings {
        enabled: config.imposters.bvimposters_bake_enabled,
        wrapper: non_empty(&config.imposters.bvimposters_bake_wrapper),
        impost_bin: non_empty(&config.imposters.bvimposters_impost_bin),
        impost_server: non_empty(&config.imposters.bvimposters_impost_server),
        impost_content_server: non_empty(&config.imposters.bvimposters_impost_content_server),
        impost_sdk6_adaption_layer: non_empty(
            &config.imposters.bvimposters_impost_sdk6_adaption_layer,
        ),
        queue_depth: Some(config.imposters.bvimposters_bake_queue_depth),
        timeout_secs: Some(config.imposters.bvimposters_bake_timeout_secs),
        max_failures: Some(config.imposters.bvimposters_bake_max_failures),
        quarantine_secs: Some(config.imposters.bvimposters_bake_quarantine_secs),
    }
}

/// The six write switches as `GET /writes` reports them.
pub(super) fn write_switches(config: &Config) -> shared::writes::Switches {
    shared::writes::Switches {
        content: config.writes.content_writes_enabled,
        create: config.create.create_writes_enabled,
        data: config.writes.data_writes_enabled,
        social: config.writes.social_writes_enabled,
        worlds: config.worlds.worlds_writes_enabled,
        ops: config.ops.ops_writes_enabled,
    }
}

/// The credits settings, `CREDITS_TRIAL_MODE`'s start requirements included. Only `serve` builds
/// them: `catalyst index` runs no credits job, though it still resolves every `*_FILE` it is given
/// at start, so the credits configuration and its key files belong to `serve`'s environment only.
pub(super) fn credits_settings(config: &Config) -> anyhow::Result<shop::credits::Settings> {
    let non_empty = Config::non_empty;
    shop::credits::Settings::parse(shop::credits::RawSettings {
        goal_flags_source: shop::credits::goals::flags::Source {
            url: config.shop.credits_feature_flags_url.clone(),
            origin: config.shop.credits_server_url.clone(),
            refresh: Duration::from_secs(config.shop.credits_feature_flags_refresh_secs),
        },
        writes_enabled: config.ops.ops_writes_enabled,
        admin_token: non_empty(&config.shop.catalyrst_credits_admin_token),
        captcha_secret: non_empty(&config.shop.credits_captcha_secret),
        captcha_verify_url: config.shop.credits_captcha_verify_url.clone(),
        stripe_secret_key: non_empty(&config.shop.stripe_secret_key),
        stripe_webhook_secret: non_empty(&config.shop.stripe_webhook_secret),
        stripe_api_base: config.shop.stripe_api_base.clone(),
        credits_currency: config.shop.credits_currency.clone(),
        marketplace_markup_bps: config.shop.marketplace_markup_bps,
        mana_price_max_staleness_secs: {
            if let Some(warning) = config.price_staleness_warning() {
                tracing::warn!("{warning}");
            }
            config.shop.mana_price_max_staleness_secs
        },
        credits_usd_pegged_feed_max_age_secs: config.shop.credits_usd_pegged_feed_max_age_secs,
        checkout_fulfillment_mode: config.shop.checkout_fulfillment_mode.clone(),
        require_purchase_intent: config.shop.credits_require_purchase_intent,
        checkout_worker_interval_secs: config.shop.checkout_worker_interval_secs,
        checkout_max_attempts: config.shop.checkout_max_attempts,
        escrow_lock_days: config.shop.escrow_lock_days,
        mock_fulfillment: config.shop.credits_mock_fulfillment,
        mock_card: config.shop.credits_mock_card,
        credits_signer_key: non_empty(&config.shop.credits_signer_key),
        credits_manager_contract: non_empty(&config.shop.credits_manager_contract),
        credits_onchain_authorize_enabled: config.shop.credits_onchain_authorize_enabled,
        checkout_success_url: config.shop.credits_checkout_success_url.clone(),
        checkout_cancel_url: config.shop.credits_checkout_cancel_url.clone(),
        checkout_success_url_builder: non_empty(&config.shop.credits_checkout_success_url_builder),
        checkout_cancel_url_builder: non_empty(&config.shop.credits_checkout_cancel_url_builder),
        usage_index_enabled: config.shop.credits_usage_index_enabled,
        usage_rpc_url: non_empty(&config.economy.rpc_endpoint_polygon),
        usage_chain_id: config.economy.collections_chain_id as i64,
        usage_start_block: config.shop.credits_usage_start_block,
        topup_start_block: config.shop.credits_topup_start_block,
        topups_enabled: Config::non_empty(&config.economy.catalyrst_economy_admin_token).is_some()
            && config.economy.meta_tx_broadcast_enabled
            && Config::non_empty(&config.economy.relayer_private_key).is_some(),
        topup_min_confirmations: config.shop.credits_topup_min_confirmations,
        usage_interval_secs: config.shop.credits_usage_index_interval_secs,
        usage_requests_per_pass: config.shop.credits_usage_index_requests_per_pass,
        usage_replay_from_block: config.shop.credits_usage_replay_from_block,
        usage_stop_block: config.shop.credits_usage_stop_block,
        credit_ttl_seconds: config.shop.credits_ttl_seconds,
        credit_cap_headroom_pct: config.shop.credits_cap_headroom_pct,
        credit_buffer_bps: config.shop.credits_oracle_buffer_bps,
        credits_revoker_key: non_empty(&config.shop.credits_revoker_key),
        trial_mode: config.shop.credits_trial_mode,
        trial_buyers: non_empty(&config.shop.credits_trial_buyers),
        shop_base_url: config.shop.shop_base_url.clone(),
    })
    .context("CREDITS_* / STRIPE_* / CHECKOUT_* configuration")
}

pub(super) fn comms_settings(config: &Config) -> anyhow::Result<comms::gatekeeper::Settings> {
    let non_empty = Config::non_empty;
    comms::gatekeeper::Settings::resolve(comms::gatekeeper::RawSettings {
        livekit_host: config.social.livekit_host.clone(),
        livekit_api_host: non_empty(&config.social.livekit_api_host),
        livekit_ws_url: non_empty(&config.social.livekit_ws_url),
        livekit_api_key: non_empty(&config.social.livekit_api_key).unwrap_or_default(),
        livekit_api_secret: non_empty(&config.social.livekit_api_secret).unwrap_or_default(),
        livekit_allow_dev_creds: config.social.livekit_allow_dev_creds,
        private_messages_room_id: config.social.private_messages_room_id.clone(),
        authoritative_server_address: non_empty(&config.social.authoritative_server_address),
        moderator_token: non_empty(&config.social.moderator_token),
        moderator_addresses: config.social.platform_user_moderators.clone(),
        gatekeeper_auth_token: non_empty(&config.social.comms_gatekeeper_auth_token),
        land_operators_url: non_empty(&config.social.comms_land_operators_url),
        lease_authorizations_url: Some(config.social.lease_authorizations_url.clone()),
        voice_connection_interrupted_ttl_ms: config.social.voice_chat_connection_interrupted_ttl_ms,
        voice_initial_connection_ttl_ms: config.social.voice_chat_initial_connection_ttl_ms,
        community_voice_no_moderator_ttl_ms: config.social.community_voice_chat_no_moderator_ttl_ms,
        voice_expired_batch_size: config.social.voice_chat_expired_batch_size,
        commit_hash: config.node.commit_hash.clone(),
    })
    .context("LIVEKIT_* / COMMS_* configuration")
}

/// The referral settings `social_states` passes on, including the backend
/// proxies trusted to supply `X-Real-IP` for referral quotas.
pub(super) fn referral_settings(config: &Config) -> communities::referral::Settings {
    communities::referral::Settings {
        trusted_proxy_cidrs: config.node.http_trusted_proxy_cidrs.clone(),
        max_ip_matches: config.social.referral_max_ip_matches,
        suspicious_window_ms: config.social.referral_suspicious_window_ms,
        slack: communities::referral::SlackSettings {
            token: config.social.referral_slack_token.clone(),
            dashboard: config.social.referral_metabase_dashboard.clone(),
            environment: config.social.social_environment.clone().unwrap_or_default(),
        },
        min_login_days: config.social.referral_min_login_days,
        profile_url: config.social.referral_profile_url.clone(),
        reward_server_url: config.social.referral_reward_server_url.clone(),
        reward_keys: [
            (
                5,
                &config.social.rewards_api_key_by_referral_invited_users_5,
            ),
            (
                10,
                &config.social.rewards_api_key_by_referral_invited_users_10,
            ),
            (
                20,
                &config.social.rewards_api_key_by_referral_invited_users_20,
            ),
            (
                25,
                &config.social.rewards_api_key_by_referral_invited_users_25,
            ),
            (
                30,
                &config.social.rewards_api_key_by_referral_invited_users_30,
            ),
            (
                50,
                &config.social.rewards_api_key_by_referral_invited_users_50,
            ),
            (
                60,
                &config.social.rewards_api_key_by_referral_invited_users_60,
            ),
            (
                75,
                &config.social.rewards_api_key_by_referral_invited_users_75,
            ),
        ]
        .into_iter()
        .map(|(tier, key)| (tier, key.clone().unwrap_or_default()))
        .collect(),
    }
}
