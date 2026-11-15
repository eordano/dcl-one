//! Scene effects responsibilities.
use super::lookup::raw_world;
use super::*;

/// The request context upstream derives a room name from
/// (`livekit.getRoomName(realmName, { isWorld, sceneId })`), beside the
/// `place_id` the endpoints are keyed on.
pub struct RoomContext {
    pub place_id: String,
    pub realm_name: Option<String>,
    pub scene_id: Option<String>,
    pub parcel: Option<String>,
}

impl RoomContext {
    pub fn from_metadata(metadata: &Value, place_id: &str) -> Self {
        Self {
            place_id: place_id.to_string(),
            realm_name: realm_name_from_metadata(metadata),
            scene_id: meta_str(metadata, "sceneId"),
            parcel: meta_str(metadata, "parcel"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum RoomPlan {
    Skip,
    Scene(String),
    World {
        world: String,
        scene_id: Option<String>,
    },
    FromPlace,
}

pub(super) fn plan(ctx: &RoomContext) -> RoomPlan {
    let Some(realm) = ctx.realm_name.as_deref() else {
        return RoomPlan::FromPlace;
    };
    if is_world_realm_name(realm) {
        return RoomPlan::World {
            world: realm.to_string(),
            scene_id: ctx.scene_id.clone(),
        };
    }
    match ctx.scene_id.as_deref() {
        Some(scene_id) => RoomPlan::Scene(gatekeeper_scene_room(realm, scene_id)),
        None => RoomPlan::Skip,
    }
}

/// The world branch keys on the `.eth` reading of the realm name, where
/// upstream decides `isWorld` from the signed realm's hostname.
pub(super) fn context_matches_place(ctx: &RoomContext, place: &PlaceInfo) -> bool {
    match ctx.realm_name.as_deref() {
        None => true,
        Some(realm) if is_world_realm_name(realm) => {
            place.world
                && place
                    .world_name
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(realm))
        }
        Some(_) => {
            !place.world
                && ctx.parcel.as_deref().is_some_and(|parcel| {
                    place.positions.iter().any(|p| p == parcel)
                        || place.base_position.as_deref() == Some(parcel)
                })
        }
    }
}

/// Upstream reads the place out of the same signed metadata it names the
/// room from; our endpoints take `place_id` from the request, so the two are
/// tied back together here, else an admin of one place could aim the LiveKit
/// side effects at another scene's room.
async fn ensure_place_matches_context(
    ctx: &RoomContext,
    place: &PlaceLookup<'_>,
) -> Result<(), ApiError> {
    let row = place.get().await.map_err(|error| {
        tracing::error!(error, place_id = %ctx.place_id, "places lookup failed while binding a mutation to its room");
        service_unavailable(PLACE_LOOKUP_UNAVAILABLE_MSG)
    })?;
    match row {
        Some(row) if context_matches_place(ctx, row) => Ok(()),
        _ => Err(ApiError::bad_request(PLACE_MISMATCH_MSG)),
    }
}

async fn world_rooms(
    state: &CommsState,
    world: &str,
    scene_id: Option<&str>,
) -> Result<Vec<String>, ApiError> {
    let world_name = raw_world(world).ok_or_else(|| ApiError::bad_request("Invalid realm name"))?;
    let mut rooms = vec![crate::rooms::worlds_service_world_room(&world_name)];
    // A catalogue WORLD can use a legacy arbitrary name without `.eth`.
    // Its known world scope must never fall through to a Genesis room.
    let scene = match scene_id {
        Some(raw) if !is_world_realm_name(raw) => {
            let parsed = SceneId::parse(raw).map_err(|e| ApiError::bad_request(e.to_string()))?;
            if parsed.as_str() != raw {
                return Err(ApiError::bad_request("Invalid scene id"));
            }
            Some(parsed)
        }
        _ => state
            .scenes
            .world_active_scene(&world_name)
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?
            .map(|dep| dep.scene_id),
    };
    if let Some(scene) = scene {
        let resolved = state
            .scenes
            .resolve(RoomRequest::WorldScene {
                world: &world_name,
                scene: &scene,
            })
            .await
            .map_err(|e| match e {
                ResolveError::SceneNotInWorld { .. } => ApiError::not_found(e.to_string()),
                _ => ApiError::internal(e.to_string()),
            })?;
        rooms.push(resolved.room);
    }
    Ok(rooms)
}

/// The LiveKit rooms a ban/admin mutation must reflect into, resolved before
/// the database write so a failure to name the room cannot leave the two out
/// of sync.
pub async fn resolve_rooms(
    state: &CommsState,
    ctx: &RoomContext,
    place: &PlaceLookup<'_>,
) -> Result<Vec<String>, ApiError> {
    if !state.livekit.configured {
        return Ok(Vec::new());
    }
    match plan(ctx) {
        RoomPlan::Skip => Ok(Vec::new()),
        RoomPlan::Scene(room) => {
            if !matches!(
                crate::rooms::parse_room(&room),
                Some(crate::rooms::ParsedRoom::Local(_))
            ) {
                return Err(ApiError::bad_request("Invalid shared realm or scene id"));
            }
            ensure_place_matches_context(ctx, place).await?;
            Ok(vec![room])
        }
        RoomPlan::World { world, scene_id } => {
            ensure_place_matches_context(ctx, place).await?;
            world_rooms(state, &world, scene_id.as_deref()).await
        }
        RoomPlan::FromPlace => Ok(match place.get().await.ok().flatten() {
            Some(row) if row.world => match row.world_name.as_deref() {
                Some(world) => world_rooms(state, world, None).await?,
                None => Vec::new(),
            },
            _ => Vec::new(),
        }),
    }
}

#[derive(Clone, Copy)]
enum Op {
    Append,
    Remove,
}

async fn mutate(state: &CommsState, rooms: &[String], field: &str, address: &Address, op: Op) {
    if !state.livekit.configured || rooms.is_empty() {
        return;
    }
    let client = state.room_service();
    futures::stream::iter(rooms)
        .for_each_concurrent(KICK_CONCURRENCY, |room| {
            let client = &client;
            async move {
                let result = match op {
                    Op::Append => client.append_to_room_metadata_array(room, field, address.as_str()).await,
                    Op::Remove => client.remove_from_room_metadata_array(room, field, address.as_str()).await,
                };
                if let Err(error) = result {
                    tracing::warn!(%error, room = %room, field, address = %address, "failed to sync scene room metadata (best-effort)");
                }
            }
        })
        .await;
}

pub async fn add_ban(state: &CommsState, rooms: &[String], address: &Address) {
    mutate(state, rooms, BANNED_ADDRESSES_FIELD, address, Op::Append).await;
}

pub async fn remove_ban(state: &CommsState, rooms: &[String], address: &Address) {
    mutate(state, rooms, BANNED_ADDRESSES_FIELD, address, Op::Remove).await;
}

pub async fn add_admin(state: &CommsState, rooms: &[String], address: &Address) {
    mutate(state, rooms, SCENE_ADMINS_FIELD, address, Op::Append).await;
}

pub async fn remove_admin(state: &CommsState, rooms: &[String], address: &Address) {
    mutate(state, rooms, SCENE_ADMINS_FIELD, address, Op::Remove).await;
}

/// Revocation takes effect in the running room: the banned wallet is removed
/// from every room the mutation named (best-effort, logged).
pub(crate) async fn kick(state: &CommsState, rooms: &[String], address: &Address) {
    if !state.livekit.configured || rooms.is_empty() {
        return;
    }
    let client = state.room_service();
    futures::stream::iter(rooms)
        .for_each_concurrent(KICK_CONCURRENCY, |room| {
            let client = &client;
            async move {
                if let Err(error) = client.remove_participant(room, address.as_str()).await {
                    tracing::warn!(%error, room = %room, address = %address, "failed to kick banned participant (best-effort)");
                }
            }
        })
        .await;
}

/// A complete inventory, or an explicit failure. No refusal is an empty scan.
pub(crate) async fn live_world_rooms(
    state: &CommsState,
    world: &WorldName,
) -> Result<Vec<String>, String> {
    let rooms = state
        .room_service()
        .list_rooms()
        .await
        .map_err(|error| error.to_string())?;
    Ok(inventory_world_rooms(world, &rooms))
}

fn inventory_world_rooms(world: &WorldName, rooms: &[String]) -> Vec<String> {
    let lobby = crate::rooms::worlds_service_world_room(world);
    rooms
        .iter()
        .filter(|room| {
            **room == lobby
                || crate::rooms::decode_world_scene_room(room)
                    .is_some_and(|(owner, _)| owner == *world)
        })
        .cloned()
        .collect()
}

/// Recheck against a durable edit cutoff. Shared-secret evidence is signed by
/// the gatekeeper; a later LiveKit reconnect does not make an old token current.
pub(crate) async fn readmit_world_rooms_in_inventory(
    state: &CommsState,
    world: &WorldName,
    cutoff: Option<chrono::NaiveDateTime>,
    inventory: &[String],
) -> Result<(), String> {
    let now = chrono::Utc::now().naive_utc();
    let admission = state
        .policy
        .world_admission(world, now)
        .await
        .map_err(|error| format!("access setting undetermined: {error}"))?;
    let rooms = inventory_world_rooms(world, inventory);
    let (protected, undecided) = if matches!(admission, WorldAdmission::Open(_)) {
        readmit_protected(state, world, &rooms).await?
    } else {
        (BTreeSet::new(), BTreeSet::new())
    };
    let mut complete = undecided.is_empty();
    let client = state.room_service();
    for room in rooms {
        if undecided.contains(&room) {
            continue;
        }
        let participants = match client.list_participants(&room).await {
            Ok(participants) => participants,
            Err(error) => {
                complete = false;
                tracing::warn!(%error, %room, "roster unavailable; participants kept for now");
                continue;
            }
        };
        let room_scene = crate::rooms::decode_world_scene_room(&room).map(|(_, scene)| scene);
        let missing_scene = if let Some(scene) = &room_scene {
            match state.scenes.world_scene(world, scene).await {
                Ok(deployment) => deployment.is_none(),
                Err(error) => {
                    complete = false;
                    tracing::warn!(%error, %room, "scene authority unavailable");
                    continue;
                }
            }
        } else {
            false
        };
        for participant in participants {
            let Ok(address) = Address::parse(&participant.identity) else {
                continue;
            };
            let remove = if missing_scene || !matches!(admission, WorldAdmission::Open(_)) {
                true
            } else {
                match state
                    .policy
                    .can_access(world, &address, None, room_scene.as_ref(), None, now)
                    .await
                {
                    Ok(Access::Banned(_)) => true,
                    Ok(_) if protected.contains(&address) => false,
                    Ok(Access::Allowed(_)) => false,
                    Ok(Access::InvalidSecret) => cutoff.is_some_and(|cutoff| {
                        !super::super::world_mint::authorized_after(
                            &state.livekit.api_secret,
                            world,
                            &participant.identity,
                            &room,
                            participant
                                .attributes
                                .get(super::super::world_mint::AUTHORIZATION_ATTRIBUTE)
                                .map(String::as_str)
                                .or(participant.metadata.as_deref()),
                            cutoff,
                        )
                    }),
                    Ok(Access::NotAllowed) => true,
                    Err(error) => {
                        complete = false;
                        tracing::warn!(%error, %room, %address, "access undetermined; participant kept for now");
                        false
                    }
                }
            };
            if remove
                && let Err(error) = client
                    .remove_participant(&room, &participant.identity)
                    .await
            {
                complete = false;
                tracing::warn!(%error, %room, %address, "failed to evict a revoked participant");
            }
        }
    }
    if complete {
        Ok(())
    } else {
        Err("world readmission incomplete".into())
    }
}

#[cfg(test)]
pub(crate) async fn readmit_world_rooms(
    state: &CommsState,
    world: &WorldName,
    cutoff: i64,
) -> Result<bool, String> {
    let rooms = state
        .room_service()
        .list_rooms()
        .await
        .map_err(|error| error.to_string())?;
    let cutoff = Some(
        chrono::DateTime::from_timestamp(cutoff, 0)
            .map(|at| at.naive_utc())
            .unwrap_or(chrono::NaiveDateTime::MAX),
    );
    Ok(
        readmit_world_rooms_in_inventory(state, world, cutoff, &rooms)
            .await
            .is_ok(),
    )
}

#[cfg(test)]
pub(crate) async fn readmit_until_settled(
    state: &CommsState,
    world: &WorldName,
    delays: &[std::time::Duration],
) -> bool {
    let cutoff = chrono::Utc::now().timestamp();
    let mut delays = delays.iter();
    loop {
        if matches!(readmit_world_rooms(state, world, cutoff).await, Ok(true)) {
            return true;
        }
        let Some(delay) = delays.next() else {
            return false;
        };
        tokio::time::sleep(*delay).await;
    }
}

/// The wallets an access change never evicts: the owner and the grantees
/// world-wide or over any live scene of `world` (its active scene included).
/// A room whose scene lookup fails is returned as undecided: its participants stay.
async fn readmit_protected(
    state: &CommsState,
    world: &WorldName,
    rooms: &[String],
) -> Result<(BTreeSet<Address>, BTreeSet<String>), String> {
    let mut parcels = ParcelSet::new();
    if let Some(active) = state
        .scenes
        .world_active_scene(world)
        .await
        .map_err(|e| e.to_string())?
    {
        parcels.extend(active.parcels.iter().copied());
    }
    let mut undecided = BTreeSet::new();
    for room in rooms {
        let Some((_, scene)) = crate::rooms::decode_world_scene_room(room) else {
            continue;
        };
        match state.scenes.world_scene(world, &scene).await {
            Ok(Some(deployment)) => parcels.extend(deployment.parcels.iter().copied()),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, room = %room, %world, "scene lookup failed; the room's participants are kept");
                undecided.insert(room.clone());
            }
        }
    }
    let protected = state
        .policy
        .protected_addresses(world, &parcels)
        .await
        .map_err(|e| e.to_string())?;
    Ok((protected, undecided))
}
