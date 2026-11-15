//! The wallet routes of the worlds content server: `GET /wallet/contribute`
//! (signed: the worlds the signer holds a deployment or streaming grant in)
//! and `GET /wallet/{wallet}/stats` (the wallet's worlds, the space its DCL
//! NAME worlds use, its storage allowance and its quota block).

use axum::Json;
use axum::extract::{OriginalUri, Path, State};
use axum::http::{HeaderMap, Uri};
use chrono::NaiveDateTime;
use serde::Serialize;
use sqlx::Row;

use crate::auth::is_eth_address;
use crate::shared::identity::Address;
use crate::shared::signed_fetch::SignedPath;

use super::SharedState;
use super::auth::{map_auth_error_api, require_verified};
use super::http::ApiError;
use super::quota;

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "worlds/"))]
pub struct ContributorDomain {
    pub name: String,
    pub user_permissions: Vec<String>,
    pub size: String,
    pub owner: Option<String>,
    #[serde(rename = "parcelCount")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub parcel_count: i64,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "worlds/"))]
pub struct ContributableDomains {
    pub domains: Vec<ContributorDomain>,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub count: usize,
}

/// GET /wallet/contribute
pub async fn get_contributable_domains(
    State(state): State<SharedState>,
    OriginalUri(original): OriginalUri,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<ContributableDomains>, ApiError> {
    let auth = require_verified(
        &headers,
        "get",
        &SignedPath::from_uris(&original, &uri),
        &[],
        state.eip1654.as_deref(),
    )
    .await
    .map_err(map_auth_error_api)?;
    // parcelCount: 0 when any grant is world-wide, else the smallest scope.
    let rows = sqlx::query(
        "SELECT w.name, w.owner, \
                array_agg(DISTINCT wp.permission ORDER BY wp.permission) AS user_permissions, \
                (SELECT COALESCE(sum(s.size), 0) FROM worlds.world_scenes s \
                  WHERE s.world_name = w.name)::text AS size, \
                CASE WHEN bool_or(pc.n = 0) THEN 0 ELSE min(pc.n) END AS parcel_count \
           FROM worlds.world_permissions wp \
           JOIN worlds.worlds w ON w.name = wp.world_name \
           CROSS JOIN LATERAL ( \
             SELECT count(*) AS n FROM worlds.world_permission_parcels p \
              WHERE p.world_name = wp.world_name AND p.permission = wp.permission \
                AND p.address = wp.address) pc \
          WHERE wp.address = $1 AND wp.permission IN ('deployment', 'streaming') \
          GROUP BY w.name, w.owner \
          ORDER BY w.name",
    )
    .bind(auth.signer.as_str())
    .fetch_all(&state.pool)
    .await?;
    let domains: Vec<ContributorDomain> = rows
        .iter()
        .map(|r| ContributorDomain {
            name: r.get("name"),
            user_permissions: r.get("user_permissions"),
            size: r.get("size"),
            owner: r.get("owner"),
            parcel_count: r.get("parcel_count"),
        })
        .collect();
    Ok(Json(ContributableDomains {
        count: domains.len(),
        domains,
    }))
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "worlds/"))]
pub struct WorldSize {
    pub name: String,
    pub size: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "worlds/"))]
pub struct WalletStats {
    pub wallet: String,
    pub dcl_names: Vec<WorldSize>,
    pub ens_names: Vec<WorldSize>,
    pub used_space: String,
    pub max_allowed_space: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub blocked_since: Option<String>,
}

/// GET /wallet/{wallet}/stats
pub async fn get_wallet_stats(
    State(state): State<SharedState>,
    Path(wallet): Path<String>,
) -> Result<Json<WalletStats>, ApiError> {
    let address = Some(wallet.as_str())
        .filter(|w| is_eth_address(w))
        .and_then(|w| Address::parse(w).ok())
        .ok_or_else(|| {
            ApiError::bad_request(
                "Invalid request. Missing or invalid wallet in request url param.",
            )
        })?;
    // A names reader fault falls back to the stored owner.
    let held = state.held_worlds(&address).await.unwrap_or_else(|e| {
        tracing::warn!(wallet = %address.as_str(), error = %e, "wallet stats: held names unavailable");
        None
    });
    let usage = quota::wallet_usage(&state.pool, &address, held.as_deref()).await?;
    let allowance = if state.allowance.is_limited() {
        state
            .allowance
            .allowance_bytes_if_idle(&address)
            .await
            .inspect_err(|e| {
                tracing::warn!(wallet = %address.as_str(), error = %e, "wallet stats: allowance unavailable");
            })
            .ok()
            .filter(|bytes| *bytes != i64::MAX)
    } else {
        None
    };
    // Without an allowance: what the per-scene caps of its DCL worlds allow.
    let max_allowed_space = allowance.unwrap_or_else(|| {
        usage
            .dcl_names
            .iter()
            .filter_map(|w| crate::comms::world_identity::WorldName::parse(&w.name).ok())
            .map(|w| state.policy.world_size_cap(&w))
            .fold(0i64, i64::saturating_add)
    });
    let sizes = |worlds: Vec<quota::WorldUsage>| -> Vec<WorldSize> {
        worlds
            .into_iter()
            .map(|w| WorldSize {
                name: w.name,
                size: w.size.to_string(),
            })
            .collect()
    };
    let blocked_since: Option<NaiveDateTime> =
        sqlx::query_scalar("SELECT created_at FROM worlds.quota_blocks WHERE wallet = $1")
            .bind(address.as_str())
            .fetch_optional(&state.pool)
            .await?;

    Ok(Json(WalletStats {
        wallet,
        dcl_names: sizes(usage.dcl_names),
        ens_names: sizes(usage.ens_names),
        used_space: usage.used_space.to_string(),
        max_allowed_space: max_allowed_space.to_string(),
        blocked_since: blocked_since.map(|t| {
            t.and_utc()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        }),
    }))
}
