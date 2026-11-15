//! Scene lookup responsibilities.
use super::*;

pub(crate) fn meta_str(meta: &Value, key: &str) -> Option<String> {
    meta.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub fn realm_name_from_metadata(metadata: &Value) -> Option<String> {
    meta_str(metadata, "realmName").or_else(|| {
        metadata
            .get("realm")
            .and_then(|r| meta_str(r, "serverName"))
    })
}

/// The place a scene-signed request stands on: the world name for a world
/// realm, else the scene id.
pub fn place_from_metadata(meta: &Value) -> Option<String> {
    if let Some(realm) = realm_name_from_metadata(meta)
        && is_world_realm_name(&realm)
    {
        return Some(realm);
    }
    meta_str(meta, "sceneId")
}

/// The key the scene-bans listings use: a world realm keys on its scene hash
/// when the metadata carries one, else on the world name (resolved below).
pub(crate) fn listing_key_candidate(meta: &Value) -> Option<String> {
    let realm_name = realm_name_from_metadata(meta);
    let scene_id = meta_str(meta, "sceneId");
    match realm_name {
        Some(realm) if is_world_realm_name(&realm) => match scene_id {
            Some(id) if !is_world_realm_name(&id) => Some(id),
            _ => Some(realm),
        },
        _ => scene_id,
    }
}

/// Keep raw lookup keys: parsing must never turn a missing raw key into a hit.
pub(super) fn raw_world(world: &str) -> Option<WorldName> {
    WorldName::parse(world)
        .ok()
        .filter(|parsed| parsed.key() == world.to_lowercase())
}

pub(crate) async fn active_scene_id(
    state: &CommsState,
    world: &str,
) -> Result<Option<String>, ApiError> {
    let Some(world) = raw_world(world) else {
        return Ok(None);
    };
    state
        .scenes
        .world_active_scene(&world)
        .await
        .map(|dep| dep.map(|dep| dep.scene_id.to_string()))
        .map_err(|error| ApiError::internal(error.to_string()))
}

pub(crate) async fn participant_room(
    state: &CommsState,
    realm: &str,
    pointer: &str,
) -> Result<Option<ResolvedRoom>, ApiError> {
    let Ok(parcel) = Parcel::parse(pointer) else {
        return Ok(None);
    };
    if parcel.canonical() != pointer {
        return Ok(None);
    }
    let realm = if is_world_realm_name(realm) {
        let Some(world) = raw_world(realm) else {
            return Ok(None);
        };
        crate::world_identity::RealmId::World(world)
    } else {
        crate::world_identity::RealmId::Shared(realm.to_string())
    };
    match state
        .scenes
        .resolve(crate::rooms::RoomRequest::Parcel {
            realm: &realm,
            parcel,
        })
        .await
    {
        Ok(room) => Ok(Some(room)),
        Err(crate::rooms::ResolveError::NoSceneAtParcel { .. }) => Ok(None),
        Err(error) => Err(ApiError::internal(error.to_string())),
    }
}

/// A world realm with a world-shaped scene id resolves to the world's active
/// scene; an explicit id under a world realm must be a scene deployed in
/// that world (the ONE room resolver's rows; the worlds service answered 404
/// for any other, and a room is minted for active deployments only, never
/// for a replaced version or an invented id); a shared realm's id is taken
/// as given (the old gatekeeper's contract for `main`/preview realms).
pub async fn resolve_world_scene_id(
    state: &CommsState,
    realm_name: &str,
    scene_id: &str,
) -> Result<String, ApiError> {
    resolve_adapter_room(state, realm_name, scene_id)
        .await?
        .scene_id
        .map(SceneId::into_string)
        .ok_or_else(|| ApiError::internal("scene resolution returned a lobby"))
}

/// Resolve once, retaining the room and the deployment's original scene key.
/// Signed keys must already be well-formed: parsing must not trim a request
/// into a different deployment. Shared/preview realms retain their signed
/// room bytes and require no catalogue/database lookup.
pub(crate) async fn resolve_adapter_room(
    state: &CommsState,
    realm_name: &str,
    scene_id: &str,
) -> Result<ResolvedRoom, ApiError> {
    let name = WorldName::parse(realm_name)
        .map_err(|e| ApiError::bad_request(format!("Invalid realm name: {e}")))?;
    if name.display() != realm_name {
        return Err(ApiError::bad_request("Invalid realm name"));
    }
    let scene = SceneId::parse(scene_id)
        .map_err(|e| ApiError::bad_request(format!("Invalid scene id: {e}")))?;
    if scene.as_str() != scene_id {
        return Err(ApiError::bad_request("Invalid scene id"));
    }
    if !is_world_realm_name(realm_name) {
        if realm_name.contains(':') || scene_id.contains(':') {
            return Err(ApiError::bad_request("Invalid shared realm or scene id"));
        }
        return Ok(ResolvedRoom {
            realm: crate::world_identity::RealmId::Shared(realm_name.to_string()),
            scene_id: Some(scene),
            deployment: None,
            room: gatekeeper_scene_room(realm_name, scene_id),
        });
    }
    let realm = crate::world_identity::RealmId::World(name);
    let request = if scene.is_world_shaped() {
        RoomRequest::Adapter {
            realm: &realm,
            scene: &scene,
        }
    } else {
        RoomRequest::WorldScene {
            world: realm.world().unwrap(),
            scene: &scene,
        }
    };
    state
        .scenes
        .resolve(request)
        .await
        .map_err(|error| match error {
            ResolveError::SceneNotInWorld { .. } => ApiError::not_found(error.to_string()),
            ResolveError::WorldHasNoScene(_) => {
                ApiError::bad_request(format!("Failed to resolve scene ID for world {realm_name}"))
            }
            _ => ApiError::internal(error.to_string()),
        })
}

/// The scene-bans listing key: an explicit `place_id`, else the metadata's,
/// with a world name resolved to its active scene.
pub async fn resolve_listing_place_id(
    state: &CommsState,
    explicit: Option<String>,
    meta: &Value,
) -> Result<String, ApiError> {
    let candidate = explicit
        .filter(|s| !s.is_empty())
        .or_else(|| listing_key_candidate(meta))
        .ok_or_else(|| ApiError::bad_request("missing place_id query"))?;
    if !is_world_realm_name(&candidate) {
        return Ok(candidate);
    }
    match active_scene_id(state, &candidate).await? {
        Some(id) => Ok(id),
        None => Err(ApiError::bad_request(format!(
            "Failed to resolve scene ID for world {candidate}"
        ))),
    }
}

#[derive(Clone)]
pub struct PlaceInfo {
    pub world: bool,
    pub world_name: Option<String>,
    pub positions: Vec<String>,
    pub base_position: Option<String>,
}

/// `explore.place` (the mirrored catalogue) then `explore.place_world_local`
/// (worlds published here); `Ok(None)` is "no such place".
pub async fn try_load_place_info(
    pool: &PgPool,
    place_id: &str,
) -> Result<Option<PlaceInfo>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT world, world_name, raw->'positions' AS positions, base_position FROM explore.place WHERE id = $1 \
         UNION ALL \
         SELECT world, world_name, raw->'positions' AS positions, base_position FROM explore.place_world_local WHERE id = $1 \
         LIMIT 1",
    )
    .bind(place_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(|row| {
        let world: bool = row.try_get("world").unwrap_or(false);
        let world_name: Option<String> = row.try_get("world_name").ok().flatten();
        let base_position: Option<String> = row.try_get("base_position").ok();
        let mut positions: Vec<String> = Vec::new();
        if let Ok(Some(Value::Array(arr))) = row.try_get::<Option<Value>, _>("positions") {
            positions.extend(arr.iter().filter_map(|p| p.as_str().map(str::to_string)));
        }
        if positions.is_empty()
            && let Some(bp) = base_position.as_deref()
        {
            positions.push(bp.to_string());
        }
        PlaceInfo {
            world,
            world_name,
            positions,
            base_position,
        }
    }))
}

/// One place row shared by every step of a request (authz, target
/// protection, room binding); read lazily.
pub struct PlaceLookup<'a> {
    state: &'a CommsState,
    place_id: String,
    cell: tokio::sync::OnceCell<Result<Option<PlaceInfo>, String>>,
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    pub(crate) fn cached_place<'a>(
        state: &'a CommsState,
        id: &str,
        info: PlaceInfo,
    ) -> PlaceLookup<'a> {
        PlaceLookup {
            state,
            place_id: id.to_owned(),
            cell: tokio::sync::OnceCell::new_with(Some(Ok(Some(info)))),
        }
    }
}

impl<'a> PlaceLookup<'a> {
    /// A place already bound to a current deployment by the ONE resolver.
    /// Moderation requests share this footprint for policy and room effects.
    pub(crate) fn resolved(state: &'a CommsState, place_id: &str, info: PlaceInfo) -> Self {
        Self {
            state,
            place_id: place_id.to_owned(),
            cell: tokio::sync::OnceCell::new_with(Some(Ok(Some(info)))),
        }
    }

    pub fn new(state: &'a CommsState, place_id: &str) -> Self {
        Self {
            state,
            place_id: place_id.to_string(),
            cell: tokio::sync::OnceCell::new(),
        }
    }

    pub fn place_id(&self) -> &str {
        &self.place_id
    }

    /// `Err` carries the catalogue error text; callers map it to their status.
    pub async fn get(&self) -> Result<Option<&PlaceInfo>, &str> {
        self.cell
            .get_or_init(|| async {
                try_load_place_info(&self.state.pool, &self.place_id)
                    .await
                    .map_err(|e| e.to_string())
            })
            .await
            .as_ref()
            .map(Option::as_ref)
            .map_err(String::as_str)
    }
}
