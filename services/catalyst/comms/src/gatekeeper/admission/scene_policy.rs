//! Scene policy responsibilities.
use super::*;

/// The world of a realm name as the permission core keys it.
fn world_of(realm_or_name: &str) -> Option<WorldName> {
    WorldName::parse(realm_or_name).ok()
}

fn parcel_set(positions: &[String]) -> ParcelSet {
    positions
        .iter()
        .filter_map(|p| Parcel::parse(p).ok())
        .collect()
}

/// Whether `identity` may enter `world_name` (the scene `scene_id` of it,
/// when known): `Policy::can_access`, the local call that replaced the
/// worlds service's `/permissions` document. The owner, an unrestricted
/// world, an allow-listed wallet or a matching shared secret (the
/// `secret` field of the signed metadata, the scene host's contract) are
/// admitted; a ban, an allow-list miss, an nft-ownership world and a wrong
/// or missing secret are not. A store that cannot answer is a 503, never a
/// denial.
pub async fn has_world_access_permission(
    state: &CommsState,
    identity: &Address,
    world_name: &str,
    scene_id: Option<&str>,
    secret: Option<&str>,
    device_id: Option<&str>,
    caller: &str,
) -> Result<bool, ApiError> {
    let Some(world) = world_of(world_name) else {
        return Ok(false);
    };
    let scene = scene_id.and_then(|id| SceneId::parse(id).ok());
    let now = chrono::Utc::now().naive_utc();
    let admission = state
        .policy
        .world_admission(&world, now)
        .await
        .map_err(|_| service_unavailable(WORLD_ACCESS_UNAVAILABLE_MSG))?;
    if !matches!(admission, WorldAdmission::Open(_)) {
        return Ok(false);
    }
    let throttle = super::super::world_mint::SecretThrottle {
        pool: &state.pool,
        world: world.key(),
        subject: caller,
        max_attempts: state.world_mint.max_secret_attempts,
    };
    // A no-secret decision also catches an access-type change since world_admission.
    let mut access = state
        .policy
        .can_access(&world, identity, None, scene.as_ref(), device_id, now)
        .await;
    if matches!(access, Ok(Access::InvalidSecret)) || (access.is_err() && secret.is_some()) {
        let reservation = throttle
            .reserve(now)
            .await
            .map_err(|_| service_unavailable(WORLD_ACCESS_UNAVAILABLE_MSG))?
            .ok_or_else(|| ApiError::http(429, "Too many shared-secret attempts"))?;
        access = state
            .policy
            .can_access(&world, identity, secret, scene.as_ref(), device_id, now)
            .await;
        throttle
            .settle(matches!(access, Ok(Access::InvalidSecret)), reservation)
            .await;
    }
    match access {
        Ok(Access::Allowed(_)) => Ok(true),
        Ok(Access::Banned(_) | Access::NotAllowed | Access::InvalidSecret) => Ok(false),
        Err(AuthorityNotEstablished::RefusedLacksAuthority { .. }) => Ok(false),
        Err(why) => {
            tracing::error!(error = %why, world = %world, "world access could not be decided");
            Err(service_unavailable(WORLD_ACCESS_UNAVAILABLE_MSG))
        }
    }
}

#[derive(Debug, Deserialize)]
struct LeaseAuthorization {
    #[serde(default)]
    addresses: Vec<String>,
    #[serde(default)]
    plots: Vec<String>,
}

/// The upstream caches: the lease authorizations and the ENS names.
pub struct SceneCaches {
    lease_holders: TtlMap<String, Arc<Vec<LeaseAuthorization>>>,
    names: TtlMap<String, String>,
}

impl Default for SceneCaches {
    fn default() -> Self {
        Self {
            lease_holders: TtlMap::new(LEASE_AUTHORIZATIONS_TTL),
            names: TtlMap::bounded(NAMES_TTL, 50_000),
        }
    }
}

pub(super) fn parse_xy(s: &str) -> Option<(i32, i32)> {
    let mut it = s.splitn(2, ',');
    Some((
        it.next()?.trim().parse().ok()?,
        it.next()?.trim().parse().ok()?,
    ))
}

/// The LAND manager document of a Genesis parcel through the permission
/// core (`Policy::land_document`: the `land_authz` index once its fold is
/// published, else `COMMS_LAND_OPERATORS_URL`). Upstream's `getLandOperators`
/// re-throws on a transport error and on any non-2xx, so a source fault
/// reaches the caller as a rejection and never as "this parcel has no
/// operators". Without any source v2 has no LAND evidence at all: that is
/// the old "places pool unset" degraded mode (no extra address is protected
/// on Genesis places), logged, not a 503.
pub(super) async fn try_fetch_land_operators(
    state: &CommsState,
    parcel: &str,
) -> Result<Option<Arc<Option<LandOperators>>>, ApiError> {
    let Some((x, y)) = parse_xy(parcel) else {
        return Ok(None);
    };
    match state.policy.land_document(Parcel { x, y }).await {
        Ok(Some(doc)) => Ok(Some(doc)),
        Ok(None) => {
            tracing::debug!(
                parcel,
                "no LAND operator source (land-authz index not ready, COMMS_LAND_OPERATORS_URL unset); no LAND operator is protected"
            );
            Ok(None)
        }
        Err(e) => {
            tracing::warn!(error = %e, parcel, "LAND operators could not be read");
            Err(service_unavailable(LAND_OPERATORS_UNAVAILABLE_MSG))
        }
    }
}

/// The addresses a place's other permission surfaces grant: the world's
/// owner and deployment/streaming wallets, or the parcel's LAND owner and
/// operators. A surface that cannot be read is an error, never an empty set
/// (an empty set here reads as "nobody is protected").
pub async fn try_get_extra_addresses(
    state: &CommsState,
    place: &PlaceInfo,
) -> Result<BTreeSet<String>, ApiError> {
    let mut extra: BTreeSet<String> = BTreeSet::new();
    if place.world {
        let Some(world) = place.world_name.as_deref().and_then(world_of) else {
            return Ok(extra);
        };
        // The known owner plus every deployment/streaming grantee whose grant
        // is world-wide or touches the place's parcels (the old
        // `/permissions/{perm}/parcels` listings plus the document's owner).
        let protected = state
            .policy
            .protected_addresses(&world, &parcel_set(&place.positions))
            .await
            .map_err(|why| {
                tracing::error!(error = %why, world = %world, "protected addresses could not be read");
                service_unavailable(WORLD_PERMISSIONS_UNAVAILABLE_MSG)
            })?;
        extra.extend(protected.into_iter().map(|a| a.as_str().to_string()));
    } else {
        let parcel = place
            .base_position
            .clone()
            .or_else(|| place.positions.first().cloned());
        if let Some(parcel) = parcel
            && let Some(doc) = try_fetch_land_operators(state, &parcel).await?
            && let Some(ops) = doc.as_ref()
        {
            extra.extend(ops.addresses());
        }
    }
    Ok(extra)
}

/// The listing's best-effort variant: an outage reads as no extra address.
pub async fn get_extra_addresses(state: &CommsState, place: &PlaceInfo) -> BTreeSet<String> {
    try_get_extra_addresses(state, place)
        .await
        .unwrap_or_default()
}

/// Holders of a linker-server lease over any of `parcels`, from the document
/// at `LEASE_AUTHORIZATIONS_URL` (the mirror posture's: Decentraland's
/// linker-server-authorizations list); no URL or an unreadable document
/// reads as no holder, as upstream.
pub async fn get_lease_holders_for_parcels(
    state: &CommsState,
    parcels: &[String],
) -> BTreeSet<String> {
    match try_get_lease_holders_for_parcels(state, parcels).await {
        Ok(holders) => holders,
        Err(error) => {
            tracing::debug!(error = %error.message, "lease authorizations unavailable; no lease holder is listed");
            BTreeSet::new()
        }
    }
}

/// Metadata refresh must preserve its previous roles when a configured reader
/// fails. Other listings retain the best-effort wrapper above.
pub(crate) async fn try_get_lease_holders_for_parcels(
    state: &CommsState,
    parcels: &[String],
) -> Result<BTreeSet<String>, ApiError> {
    let mut holders: BTreeSet<String> = BTreeSet::new();
    let Some(url) = state.settings.lease_authorizations_url.as_deref() else {
        return Ok(holders);
    };
    if parcels.is_empty() {
        return Ok(holders);
    }
    let fetched = state
        .caches
        .lease_holders
        .get_or_fetch("leases".to_string(), || async {
            let resp = state
                .http
                .get(url)
                .timeout(UPSTREAM_TIMEOUT)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if !resp.status().is_success() {
                return Err(format!("lease authorizations returned {}", resp.status()));
            }
            resp.json::<Vec<LeaseAuthorization>>()
                .await
                .map(Arc::new)
                .map_err(|e| e.to_string())
        })
        .await;
    let auths = match fetched {
        Ok(auths) => auths,
        Err(e) => {
            tracing::debug!(error = %e, "lease authorizations unavailable");
            return Err(service_unavailable(
                "Lease authorizations are temporarily unavailable",
            ));
        }
    };
    let parcel_set: BTreeSet<&str> = parcels.iter().map(String::as_str).collect();
    for auth in auths.iter() {
        if auth
            .plots
            .iter()
            .any(|plot| parcel_set.contains(plot.as_str()))
        {
            holders.extend(auth.addresses.iter().map(|a| a.to_lowercase()));
        }
    }
    Ok(holders)
}

/// ENS display names for a bounded set of wallets through the marketplace
/// names reader (the old crate read the squid `ens` table). Unconfigured or
/// failing reads answer `""`, as before.
pub async fn names_for(
    state: &CommsState,
    addresses: &[String],
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Some(market) = state.marketplace.as_ref() else {
        for addr in addresses {
            out.insert(addr.to_lowercase(), String::new());
        }
        return out;
    };
    for addr in addresses {
        let addr = addr.to_lowercase();
        let name = state
            .caches
            .names
            .get_or_fetch(addr.clone(), || async {
                match crate::chain_index::graph::owned_names(market, &addr).await {
                    Ok(names) => Ok::<String, ()>(names.first().map(|n| n.name.clone()).unwrap_or_default()),
                    Err(e) => {
                        tracing::warn!(error = %e, "names resolve failed; falling back to an empty name");
                        Ok(String::new())
                    }
                }
            })
            .await
            .unwrap_or_default();
        out.insert(addr, name);
    }
    out
}

/// An explicit active `scene_admins` row, else ownership of the place: the
/// world's NAME (personal rule, the stored owner, then the name authority)
/// for a world, the LAND (a parcel directly or through its estate) for a
/// Genesis place, all through `Policy::is_admin`. Without a marketplace
/// reader a non-admin caller is refused (the old crate's "only explicit
/// scene admins can moderate" degrade); a reader that cannot be asked is
/// 503, never "not an owner".
pub async fn scene_owner_or_admin_decision(
    state: &CommsState,
    place: &PlaceLookup<'_>,
    signer: &Address,
) -> PermissionDecision {
    let place_id = place.place_id();
    let Ok(scene) = SceneId::parse(place_id) else {
        return PermissionDecision::refused(NOT_OWNER_OR_ADMIN_MSG);
    };
    let place_row = match place.get().await {
        Ok(row) => row,
        Err(error) => {
            tracing::error!(
                error,
                place_id,
                "place lookup failed during the scene authz check"
            );
            return PermissionDecision::Refused(
                AuthorityNotEstablished::UndeterminedStoreUnavailable {
                    store: "explore.place",
                    reason_for_operators: error.to_string(),
                },
            );
        }
    };
    let empty = ParcelSet::new();
    let (world, parcels) = match place_row {
        Some(row) if row.world => (row.world_name.as_deref().and_then(world_of), empty),
        Some(row) => (None, parcel_set(&row.positions)),
        None => (None, empty),
    };
    let scene_ref = match world.as_ref() {
        Some(world) => SceneRef::World {
            world,
            scene: &scene,
        },
        None => SceneRef::Genesis {
            scene: &scene,
            parcels: &parcels,
        },
    };
    match state.policy.is_admin(&scene_ref, signer).await {
        PermissionDecision::Allowed => PermissionDecision::Allowed,
        PermissionDecision::Refused(AuthorityNotEstablished::RefusedLacksAuthority { .. }) => {
            PermissionDecision::refused(NOT_OWNER_OR_ADMIN_MSG)
        }
        undetermined => undetermined,
    }
}

/// Owners, admins, extra addresses and lease holders cannot be banned. A
/// protection surface that cannot be read is answered apart from a genuine
/// "not protected": 503, never "nobody is protected".
pub async fn ensure_target_not_protected(
    state: &CommsState,
    place: &PlaceLookup<'_>,
    target: &Address,
) -> Result<(), ApiError> {
    match scene_owner_or_admin_decision(state, place, target).await {
        PermissionDecision::Allowed => {
            return Err(ApiError::bad_request("Cannot ban this address"));
        }
        PermissionDecision::Refused(AuthorityNotEstablished::RefusedLacksAuthority { .. }) => {}
        PermissionDecision::Refused(why) => return Err(why.into()),
    }
    let place_id = place.place_id().to_string();
    let row = place.get().await.map_err(|error| {
        tracing::error!(error, %place_id, "places lookup failed while checking a ban target for protection");
        service_unavailable(PLACE_LOOKUP_UNAVAILABLE_MSG)
    })?;
    if let Some(row) = row {
        let (extra, leases) = tokio::join!(try_get_extra_addresses(state, row), async {
            if row.world {
                BTreeSet::new()
            } else {
                get_lease_holders_for_parcels(state, &row.positions).await
            }
        });
        let mut protected = extra?;
        protected.extend(leases);
        if protected.contains(target.as_str()) {
            return Err(ApiError::bad_request("Cannot ban this address"));
        }
    }
    Ok(())
}
