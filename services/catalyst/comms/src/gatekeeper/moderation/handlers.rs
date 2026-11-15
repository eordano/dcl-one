//! The moderation routes: scene admins and bans, the world ban-status probe,
//! and the platform bans and warnings behind the moderator gate.

use std::collections::{BTreeMap, BTreeSet};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{OriginalUri, Path, Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Address;
use crate::rooms::RoomRequest;
use crate::world_identity::{Parcel, RealmId, WorldName};

use super::context::{Context, Input};
use super::store::{BanWriteError, CreateBan, CreateWarning, LiftError, SceneAdminRow};
use crate::gatekeeper::admission::scene::{self, PlaceLookup};
use crate::gatekeeper::auth::{
    ModeratorCredentials, ModeratorMode, SCENE_SIGNER, SERVER_SIGNER, SchemaValidate,
    authorize_moderator, require_service_token, service_token_error, validate_body,
    verify_signed_fetch,
};
use crate::gatekeeper::errors::{ApiError, conflict, not_found_labeled};
use crate::gatekeeper::{CommsState, SharedState};
use crate::shared::signed_fetch::SignedPath;

fn admin_entry(a: &SceneAdminRow, name: String, can_be_removed: bool) -> Value {
    serde_json::json!({
        "id": a.id,
        "place_id": a.place_id,
        "admin": a.admin,
        "added_by": a.added_by,
        "created_at": a.created_at,
        "active": a.active,
        "name": name,
        "canBeRemoved": can_be_removed,
    })
}

fn address_entry(address: &str, name: String) -> Value {
    serde_json::json!({ "admin": address, "name": name, "canBeRemoved": false })
}

#[derive(Debug, Deserialize)]
pub struct PlaceQuery {
    pub place_id: Option<String>,
    pub admin: Option<String>,
}

fn parse_address(raw: &str, what: &str) -> Result<Address, ApiError> {
    Address::parse(raw).map_err(|_| ApiError::bad_request(format!("invalid {what}")))
}

async fn target_address(
    state: &CommsState,
    input: &Input,
    address: &str,
    name: Option<&str>,
) -> Result<Address, ApiError> {
    // Baseline anyOf accepts both and gives the address precedence.
    if let Some(raw) = input.address(address) {
        return parse_address(raw, address);
    }
    let name = name
        .and_then(|key| input.name(key))
        .filter(|name| !name.is_empty())
        .ok_or_else(|| ApiError::bad_request("Missing moderation target"))?;
    let reader = state
        .name_owners
        .as_ref()
        .ok_or_else(|| ApiError::http(503, "Name owner reader is unavailable"))?;
    reader
        .name_owner(name)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "moderation name owner lookup failed");
            ApiError::http(503, "Name owner reader is unavailable")
        })?
        .ok_or_else(|| ApiError::not_found("Name owner not found"))
}

async fn ensure_admin_target_not_privileged(
    state: &CommsState,
    place: &PlaceLookup<'_>,
    target: &Address,
) -> Result<(), ApiError> {
    let row = place
        .get()
        .await
        .map_err(|_| ApiError::http(503, "Scene place is unavailable"))?
        .ok_or_else(|| ApiError::bad_request("Scene place not found"))?;
    let extra = scene::try_get_extra_addresses(state, row).await?;
    if extra.contains(target.as_str()) {
        return Err(ApiError::bad_request(
            "Cannot modify an address with scene privileges",
        ));
    }
    Ok(())
}

/// GET /scene-admin (scene or server signed): the explicit admins plus the
/// addresses the place's other permission surfaces grant, with ENS names.
pub async fn list_admins(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(q): Query<PlaceQuery>,
) -> Result<Json<Value>, ApiError> {
    let sf = verify_signed_fetch(
        &headers,
        "get",
        &SignedPath::at(&uri, "/scene-admin"),
        &[SCENE_SIGNER, SERVER_SIGNER],
        state.eip1654(),
    )
    .await?;
    let context = if sf.metadata.get("signer").and_then(Value::as_str) == Some(SCENE_SIGNER) {
        Some(Context::resolve(&state, &sf.metadata).await?)
    } else {
        None
    };
    let place_id = match &context {
        Some(context) => {
            if q.place_id
                .as_deref()
                .is_some_and(|id| id != context.place.place_id())
            {
                return Err(ApiError::bad_request(
                    "place_id does not match the signed scene",
                ));
            }
            context.place.place_id().to_owned()
        }
        None => q
            .place_id
            .or_else(|| scene::place_from_metadata(&sf.metadata))
            .ok_or_else(|| ApiError::bad_request("missing place_id query"))?,
    };
    let admin_filter = match q.admin.as_deref() {
        Some(raw) => Some(parse_address(raw, "admin query")?),
        None => None,
    };
    let (admins, place) = tokio::try_join!(
        state
            .moderation
            .list_active_admins(&place_id, admin_filter.as_ref()),
        async {
            Ok::<_, ApiError>(match &context {
                Some(context) => {
                    context
                        .place
                        .get()
                        .await
                        .ok()
                        .flatten()
                        .map(|info| scene::PlaceInfo {
                            world: info.world,
                            world_name: info.world_name.clone(),
                            positions: info.positions.clone(),
                            base_position: info.base_position.clone(),
                        })
                }
                None => scene::try_load_place_info(&state.pool, &place_id)
                    .await
                    .ok()
                    .flatten(),
            })
        }
    )?;
    let (extra_addresses, lease_holders) = match place {
        Some(place) => tokio::join!(scene::get_extra_addresses(&state, &place), async {
            if place.world {
                BTreeSet::new()
            } else {
                scene::get_lease_holders_for_parcels(&state, &place.positions).await
            }
        }),
        None => (BTreeSet::new(), BTreeSet::new()),
    };
    let mut all_addresses: BTreeSet<String> =
        admins.iter().map(|a| a.admin.to_lowercase()).collect();
    all_addresses.extend(extra_addresses.iter().cloned());
    all_addresses.extend(lease_holders.iter().cloned());
    let lookup: Vec<String> = all_addresses.iter().cloned().collect();
    let names = scene::names_for(&state, &lookup).await;
    let name_of = |addr: &str| names.get(addr).cloned().unwrap_or_default();
    let mut body: Vec<Value> = Vec::new();
    for a in &admins {
        let admin_lc = a.admin.to_lowercase();
        body.push(admin_entry(
            a,
            name_of(&admin_lc),
            !extra_addresses.contains(&admin_lc),
        ));
    }
    for address in &extra_addresses {
        body.push(address_entry(address, name_of(address)));
    }
    for address in &lease_holders {
        body.push(address_entry(address, name_of(address)));
    }
    Ok(Json(Value::Array(body)))
}

/// POST /scene-admin (scene signed): the signer must be an owner or admin of
/// the signed scene; its canonical room gets the admin appended.
pub async fn add_admin(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let sf = verify_signed_fetch(
        &headers,
        "post",
        &SignedPath::at(&uri, "/scene-admin"),
        &[SCENE_SIGNER],
        state.eip1654(),
    )
    .await?;
    let input = Input::parse(&body, query, "admin", Some("name"), false)?;
    let context = Context::resolve(&state, &sf.metadata).await?;
    context.check_selectors(&input)?;
    let place = &context.place;
    scene::scene_owner_or_admin_decision(&state, place, &sf.signer)
        .await
        .into_result()?;
    let admin = target_address(&state, &input, "admin", Some("name")).await?;
    ensure_admin_target_not_privileged(&state, place, &admin).await?;
    match scene::scene_owner_or_admin_decision(&state, place, &admin).await {
        crate::PermissionDecision::Allowed => {
            return Err(ApiError::bad_request("Cannot add this address as an admin"));
        }
        crate::PermissionDecision::Refused(
            crate::AuthorityNotEstablished::RefusedLacksAuthority { .. },
        ) => {}
        crate::PermissionDecision::Refused(why) => return Err(why.into()),
    }
    if state
        .moderation
        .is_scene_banned(context.ban_key.as_str(), &admin)
        .await?
    {
        return Err(ApiError::bad_request(
            "Cannot add a banned address as an admin",
        ));
    }
    state
        .moderation
        .add_admin(place.place_id(), &admin, &sf.signer)
        .await?;
    scene::add_admin(&state, &context.rooms, &admin).await;
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /scene-admin JSON {admin}; legacy query targets remain bound to
/// the same verified scene identity.
pub async fn remove_admin(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let sf = verify_signed_fetch(
        &headers,
        "delete",
        &SignedPath::at(&uri, "/scene-admin"),
        &[SCENE_SIGNER],
        state.eip1654(),
    )
    .await?;
    let input = Input::parse(&body, query, "admin", None, true)?;
    let context = Context::resolve(&state, &sf.metadata).await?;
    context.check_selectors(&input)?;
    let place = &context.place;
    scene::scene_owner_or_admin_decision(&state, place, &sf.signer)
        .await
        .into_result()?;
    let admin = target_address(&state, &input, "admin", None).await?;
    ensure_admin_target_not_privileged(&state, place, &admin).await?;
    state
        .moderation
        .admin_decision(place.place_id(), &admin)
        .await
        .into_result()
        .map_err(|error| match error {
            crate::AuthorityNotEstablished::RefusedLacksAuthority { .. } => {
                ApiError::bad_request("The specified admin is not active")
            }
            why => why.into(),
        })?;
    state
        .moderation
        .remove_admin(place.place_id(), &admin)
        .await?;
    scene::remove_admin(&state, &context.rooms, &admin).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct BanQuery {
    pub place_id: Option<String>,
    pub banned_address: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

const MAX_LIMIT: i64 = 100;

pub fn limit_or_max(requested: Option<i64>, max: i64) -> i64 {
    match requested {
        Some(n) if n > 0 && n <= max => n,
        _ => max,
    }
}

fn pagination(limit: Option<i64>, offset: Option<i64>) -> (i64, i64) {
    (
        limit_or_max(limit, MAX_LIMIT),
        offset.filter(|o| *o >= 0).unwrap_or(0),
    )
}

/// The paginated envelope the gatekeeper publishes (1-based `page`).
#[derive(Debug, Serialize)]
pub struct PaginatedResponse<T> {
    pub results: Vec<T>,
    pub total: i64,
    pub page: i64,
    pub pages: i64,
    pub limit: i64,
}

impl<T> PaginatedResponse<T> {
    pub fn one_based(results: Vec<T>, total: i64, limit: i64, offset: i64) -> Self {
        let page = if limit > 0 { offset / limit } else { 0 } + 1;
        let pages = if limit > 0 {
            (total + limit - 1) / limit
        } else {
            0
        };
        Self {
            results,
            total,
            page,
            pages,
            limit,
        }
    }
}

/// GET /scene-bans (scene signed): the banned wallets of the place with
/// names, keyed on the resolved scene.
pub async fn list_bans(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(q): Query<BanQuery>,
) -> Result<Json<PaginatedResponse<Value>>, ApiError> {
    let sf = verify_signed_fetch(
        &headers,
        "get",
        &SignedPath::at(&uri, "/scene-bans"),
        &[SCENE_SIGNER],
        state.eip1654(),
    )
    .await?;
    let context = Context::resolve(&state, &sf.metadata).await?;
    let place_id = context.place.place_id();
    if q.place_id.as_deref().is_some_and(|id| id != place_id) {
        return Err(ApiError::bad_request(
            "place_id does not match the signed scene",
        ));
    }
    let (limit, offset) = pagination(q.limit, q.offset);
    let (addresses, total) = state
        .moderation
        .list_scene_ban_addresses(context.ban_key.as_str(), limit, offset)
        .await?;
    let names = scene::names_for(&state, &addresses).await;
    let results: Vec<Value> = addresses
        .into_iter()
        .map(|addr| {
            let name = names.get(&addr).cloned().unwrap_or_default();
            serde_json::json!({ "bannedAddress": addr, "name": name })
        })
        .collect();
    Ok(Json(PaginatedResponse::one_based(
        results, total, limit, offset,
    )))
}

/// GET /scene-bans/addresses (scene signed).
pub async fn list_ban_addresses(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(q): Query<BanQuery>,
) -> Result<Json<PaginatedResponse<String>>, ApiError> {
    let sf = verify_signed_fetch(
        &headers,
        "get",
        &SignedPath::at(&uri, "/scene-bans/addresses"),
        &[SCENE_SIGNER],
        state.eip1654(),
    )
    .await?;
    let context = Context::resolve(&state, &sf.metadata).await?;
    let place_id = context.place.place_id();
    if q.place_id.as_deref().is_some_and(|id| id != place_id) {
        return Err(ApiError::bad_request(
            "place_id does not match the signed scene",
        ));
    }
    let (limit, offset) = pagination(q.limit, q.offset);
    let (addresses, total) = state
        .moderation
        .list_scene_ban_addresses(context.ban_key.as_str(), limit, offset)
        .await?;
    Ok(Json(PaginatedResponse::one_based(
        addresses, total, limit, offset,
    )))
}

/// POST /scene-bans (scene signed): owner-or-admin, the target not protected,
/// the rooms resolved before the write, then kick + room metadata.
pub async fn ban_user(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let sf = verify_signed_fetch(
        &headers,
        "post",
        &SignedPath::at(&uri, "/scene-bans"),
        &[SCENE_SIGNER],
        state.eip1654(),
    )
    .await?;
    let input = Input::parse(&body, query, "banned_address", Some("banned_name"), false)?;
    let context = Context::resolve(&state, &sf.metadata).await?;
    context.check_selectors(&input)?;
    let place = &context.place;
    scene::scene_owner_or_admin_decision(&state, place, &sf.signer)
        .await
        .into_result()?;
    let target = target_address(&state, &input, "banned_address", Some("banned_name")).await?;
    scene::ensure_target_not_protected(&state, place, &target).await?;
    let mut rooms = context.rooms.clone();
    rooms.extend(context.replaced_scene_rooms(&state).await?);
    state
        .moderation
        .scene_ban(
            context.ban_key.as_str(),
            &target,
            &sf.signer,
            &rooms,
            &context.place_title,
        )
        .await?;
    scene::kick(&state, &rooms, &target).await;
    scene::add_ban(&state, &rooms, &target).await;
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /scene-bans JSON {banned_address} or {banned_name}, or a legacy
/// query target bound to the same verified scene.
pub async fn unban_user(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let sf = verify_signed_fetch(
        &headers,
        "delete",
        &SignedPath::at(&uri, "/scene-bans"),
        &[SCENE_SIGNER],
        state.eip1654(),
    )
    .await?;
    let input = Input::parse(&body, query, "banned_address", Some("banned_name"), true)?;
    let context = Context::resolve(&state, &sf.metadata).await?;
    context.check_selectors(&input)?;
    let place = &context.place;
    scene::scene_owner_or_admin_decision(&state, place, &sf.signer)
        .await
        .into_result()?;
    let target = target_address(&state, &input, "banned_address", Some("banned_name")).await?;
    state
        .moderation
        .scene_unban(context.ban_key.as_str(), &target, &context.place_title)
        .await?;
    // Admission and retry cancellation commit even while LiveKit is unavailable.
    // Inventory and derived room metadata are repaired by maintenance if this
    // best-effort synchronization cannot finish.
    let mut rooms = context.rooms.clone();
    match context.replaced_scene_rooms(&state).await {
        Ok(replaced) => rooms.extend(replaced),
        Err(error) => {
            tracing::warn!(error = %error.message, "unban committed; replaced-room metadata awaits maintenance")
        }
    }
    match state
        .moderation
        .is_scene_banned(context.ban_key.as_str(), &target)
        .await
    {
        Ok(false) => {
            scene::remove_ban(&state, &rooms, &target).await;
            // A re-ban may have committed and appended metadata while this
            // request waited for an SFU write. Restore that newer authoritative
            // ban instead of leaving the delayed unban's removal behind.
            match state
                .moderation
                .is_scene_banned(context.ban_key.as_str(), &target)
                .await
            {
                Ok(true) => scene::add_ban(&state, &rooms, &target).await,
                Ok(false) => {}
                Err(error) => tracing::warn!(%error, "unban metadata recheck awaits maintenance"),
            }
        }
        Ok(true) => {}
        Err(error) => {
            tracing::warn!(%error, "unban committed; metadata synchronization awaits maintenance")
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

pub const WORLD_BAN_STATUS_PATH: &str =
    "/worlds/{world_name}/parcels/{base_parcel}/users/{address}/ban-status";

/// GET the ban status of `address` at a world parcel, behind the service
/// token. A completed lookup answers `isBanned`; an undetermined lookup
/// answers 503, so an unavailable decision is distinguishable from a ban.
pub async fn world_ban_check(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path((world_name, base_parcel, address)): Path<(String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    require_service_token(state.settings.gatekeeper_auth_token.as_deref(), &headers)
        .map_err(service_token_error)?;
    let unavailable =
        || ApiError::http(503, "World ban status could not be decided; retry shortly.");
    let address = Address::parse(&address).map_err(|_| unavailable())?;
    let world = WorldName::parse(&world_name).map_err(|_| unavailable())?;
    let parcel = Parcel::parse(&base_parcel).map_err(|_| unavailable())?;
    let realm = RealmId::World(world);
    let resolved = state.scenes.resolve(RoomRequest::Parcel { realm: &realm, parcel })
        .await.map_err(|error| {
            tracing::error!(%error, world = %world_name, parcel = %base_parcel, "world ban check: scene resolution unavailable");
            unavailable()
        })?;
    let place_id = resolved.ban_key().ok_or_else(unavailable)?;
    let is_banned = match state
        .moderation
        .is_scene_banned(place_id.as_str(), &address)
        .await
    {
        Ok(banned) => banned,
        Err(e) => {
            tracing::error!(error = %e, world = %world_name, parcel = %base_parcel, %address, place_id = place_id.as_str(), "world ban check: ban store query failed");
            return Err(unavailable());
        }
    };
    Ok(Json(serde_json::json!({ "isBanned": is_banned })))
}

#[derive(Debug, Deserialize, Default)]
pub struct ModeratorQuery {
    pub moderator: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BanPlayerBody {
    pub reason: String,
    pub duration: Option<i64>,
    #[serde(rename = "customMessage")]
    pub custom_message: Option<String>,
}

impl SchemaValidate for BanPlayerBody {
    fn schema_validate(value: &Value) -> Result<(), String> {
        let obj = value
            .as_object()
            .ok_or_else(|| "must be an object".to_string())?;
        for key in obj.keys() {
            if !matches!(key.as_str(), "reason" | "duration" | "customMessage") {
                return Err(format!("additional property not allowed: {key}"));
            }
        }
        match obj.get("reason").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => {}
            _ => return Err("reason must be a non-empty string".to_string()),
        }
        if let Some(d) = obj.get("duration") {
            match d.as_f64() {
                Some(n) if n > 0.0 => {}
                _ => return Err("duration must be a number greater than 0".to_string()),
            }
        }
        if let Some(m) = obj.get("customMessage")
            && !m.is_string()
        {
            return Err("customMessage must be a string".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
pub struct WarnPlayerBody {
    pub reason: String,
}

impl SchemaValidate for WarnPlayerBody {
    fn schema_validate(value: &Value) -> Result<(), String> {
        let obj = value
            .as_object()
            .ok_or_else(|| "must be an object".to_string())?;
        for key in obj.keys() {
            if key != "reason" {
                return Err(format!("additional property not allowed: {key}"));
            }
        }
        match obj.get("reason").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => Ok(()),
            _ => Err("reason must be a non-empty string".to_string()),
        }
    }
}

pub(in crate::gatekeeper) fn moderator_creds(state: &CommsState) -> ModeratorCredentials<'_> {
    ModeratorCredentials {
        token: state.settings.moderator_token.as_deref(),
        addresses: &state.settings.moderator_addresses,
    }
}

/// GET /users/{address}/bans: public; the device id is never on this shape.
pub async fn get_user_ban_status(
    State(state): State<SharedState>,
    Path(address): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let address = Address::parse(&address).map_err(|_| ApiError::bad_request("Invalid address"))?;
    let status = state.moderation.ban_status(&address).await?;
    let data = serde_json::to_value(status).unwrap_or(serde_json::json!({ "isBanned": false }));
    Ok(Json(serde_json::json!({ "data": data })))
}

/// POST /users/{address}/bans (moderator): one in-force ban per wallet, the
/// recorded device snapshotted, and the wallet kicked from every live room.
pub async fn post_user_ban(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(address): Path<String>,
    Query(q): Query<ModeratorQuery>,
    body_bytes: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let banned_by = authorize_moderator(
        &moderator_creds(&state),
        &headers,
        "post",
        &SignedPath::at(&uri, format!("/users/{address}/bans")),
        ModeratorMode::Write,
        q.moderator.as_deref(),
        state.eip1654(),
    )
    .await?;
    let address = Address::parse(&address).map_err(|_| ApiError::bad_request("Invalid address"))?;
    let content_type = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok());
    let body: BanPlayerBody = validate_body(content_type, &body_bytes)?;
    let ban = state
        .moderation
        .create_ban_with_recorded_device(CreateBan {
            banned_address: address.clone(),
            banned_by,
            reason: body.reason,
            custom_message: body.custom_message,
            banned_device_id: None,
            duration_ms: body.duration,
        })
        .await
        .map_err(|e| match e {
            BanWriteError::AlreadyBanned(addr) => {
                conflict(format!("Player is already banned: {addr}"))
            }
            BanWriteError::Db(e) => e,
        })?;
    let data = serde_json::to_value(ban).unwrap_or(Value::Null);
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "data": data })),
    ))
}

/// DELETE /users/{address}/bans (moderator).
pub async fn delete_user_ban(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(address): Path<String>,
    Query(q): Query<ModeratorQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let lifted_by = authorize_moderator(
        &moderator_creds(&state),
        &headers,
        "delete",
        &SignedPath::at(&uri, format!("/users/{address}/bans")),
        ModeratorMode::Write,
        q.moderator.as_deref(),
        state.eip1654(),
    )
    .await?;
    let address = Address::parse(&address).map_err(|_| ApiError::bad_request("Invalid address"))?;
    state
        .moderation
        .lift_ban(&address, &lifted_by)
        .await
        .map_err(|e| match e {
            LiftError::NotFound(addr) => {
                not_found_labeled(format!("No active ban found for player: {addr}"))
            }
            LiftError::Db(e) => e,
        })?;
    Ok(StatusCode::NO_CONTENT)
}

/// GET /users/{address}/warnings (moderator).
pub async fn get_user_warnings(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(address): Path<String>,
) -> Result<Json<Value>, ApiError> {
    authorize_moderator(
        &moderator_creds(&state),
        &headers,
        "get",
        &SignedPath::at(&uri, format!("/users/{address}/warnings")),
        ModeratorMode::Read,
        None,
        state.eip1654(),
    )
    .await?;
    let address = Address::parse(&address).map_err(|_| ApiError::bad_request("Invalid address"))?;
    let warnings = state.moderation.warnings(&address).await?;
    Ok(Json(
        serde_json::json!({ "data": serde_json::to_value(warnings).unwrap_or(Value::Array(vec![])) }),
    ))
}

/// POST /users/{address}/warnings (moderator).
pub async fn post_user_warning(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(address): Path<String>,
    Query(q): Query<ModeratorQuery>,
    body_bytes: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let warned_by = authorize_moderator(
        &moderator_creds(&state),
        &headers,
        "post",
        &SignedPath::at(&uri, format!("/users/{address}/warnings")),
        ModeratorMode::Write,
        q.moderator.as_deref(),
        state.eip1654(),
    )
    .await?;
    let address = Address::parse(&address).map_err(|_| ApiError::bad_request("Invalid address"))?;
    let content_type = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok());
    let body: WarnPlayerBody = validate_body(content_type, &body_bytes)?;
    let warning = state
        .moderation
        .create_warning(CreateWarning {
            warned_address: address,
            warned_by,
            reason: body.reason,
        })
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "data": serde_json::to_value(warning).unwrap_or(Value::Null) })),
    ))
}

/// GET /bans (moderator): every ban in force, device ids included.
pub async fn list_all_bans(
    State(state): State<SharedState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Result<axum::response::Response, ApiError> {
    authorize_moderator(
        &moderator_creds(&state),
        &headers,
        "get",
        &SignedPath::at(&uri, "/bans"),
        ModeratorMode::Read,
        None,
        state.eip1654(),
    )
    .await?;
    let bans = state.moderation.active_bans().await?;
    Ok(bans_response(&headers, &bans))
}

fn bans_response(headers: &HeaderMap, bans: &[super::store::UserBan]) -> axum::response::Response {
    use sha2::{Digest, Sha256};
    // Active bans with equal timestamps need a stable order for the body validator.
    let mut bans: Vec<_> = bans.iter().collect();
    bans.sort_by(|a, b| b.banned_at.cmp(&a.banned_at).then_with(|| a.id.cmp(&b.id)));
    let body = serde_json::to_vec(&serde_json::json!({ "data": bans })).expect("ban serialization");
    let etag = format!("\"{}\"", hex::encode(Sha256::digest(&body)));
    let matches = headers
        .get_all(axum::http::header::IF_NONE_MATCH)
        .iter()
        .any(|value| {
            value.to_str().is_ok_and(|value| {
                value.split(',').any(|tag| {
                    let tag = tag.trim();
                    tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == etag
                })
            })
        });
    let mut response = if matches {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        ([(CONTENT_TYPE, "application/json")], body).into_response()
    };
    response
        .headers_mut()
        .insert(axum::http::header::ETAG, etag.parse().expect("hex etag"));
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        "private, no-cache".parse().unwrap(),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[tokio::test]
    async fn bans_etag_conditional_get_keeps_envelope_and_empty_304() {
        use axum::body::to_bytes;
        use axum::http::header::{ETAG, IF_NONE_MATCH};
        let response = bans_response(&HeaderMap::new(), &[]);
        assert_eq!(response.status(), StatusCode::OK);
        let etag = response.headers()[ETAG].clone();
        assert_eq!(
            to_bytes(response.into_body(), 1024).await.unwrap(),
            r#"{"data":[]}"#
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            IF_NONE_MATCH,
            format!("\"other\", W/{}", etag.to_str().unwrap())
                .parse()
                .unwrap(),
        );
        let response = bans_response(&headers, &[]);
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()[ETAG], etag);
        assert!(
            to_bytes(response.into_body(), 1024)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn bans_conditional_http_still_requires_moderator_authorization() {
        use axum::{
            Router,
            http::{Request, header::IF_NONE_MATCH},
            routing::get,
        };
        use tower::ServiceExt;
        async fn endpoint(headers: HeaderMap) -> axum::response::Response {
            let creds = ModeratorCredentials {
                token: Some("fixture"),
                addresses: &[],
            };
            match authorize_moderator(
                &creds,
                &headers,
                "get",
                &SignedPath::local("/bans"),
                ModeratorMode::Read,
                None,
                None,
            )
            .await
            {
                Ok(_) => bans_response(&headers, &[]),
                Err(error) => error.into_response(),
            }
        }
        let app = Router::new().route("/bans", get(endpoint));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/bans")
                    .header(IF_NONE_MATCH, "*")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(!response.headers().contains_key(axum::http::header::ETAG));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/bans")
                    .header(IF_NONE_MATCH, "*")
                    .header("authorization", "Bearer fixture")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    }

    fn row() -> SceneAdminRow {
        SceneAdminRow {
            id: Uuid::nil(),
            place_id: "place-1".into(),
            admin: "0xADMIN".into(),
            added_by: "0xADDER".into(),
            created_at: 1_700_000_000_000,
            active: true,
        }
    }

    #[test]
    fn admin_entries_match_upstream_shape() {
        let body = Value::Array(vec![
            admin_entry(&row(), "alice.dcl.eth".into(), true),
            address_entry("0xextra", String::new()),
        ]);
        assert_eq!(body.as_array().unwrap().len(), 2);
        let e = admin_entry(&row(), "alice.dcl.eth".into(), true);
        assert_eq!(e["id"], Uuid::nil().to_string());
        assert_eq!(e["place_id"], "place-1");
        assert_eq!(e["admin"], "0xADMIN");
        assert_eq!(e["added_by"], "0xADDER");
        assert_eq!(e["created_at"], 1_700_000_000_000i64);
        assert_eq!(e["active"], true);
        assert_eq!(e["name"], "alice.dcl.eth");
        assert_eq!(e["canBeRemoved"], true);
        assert!(e.get("can_be_removed").is_none());
        let e = address_entry("0xextra", String::new());
        assert_eq!(e["admin"], "0xextra");
        assert_eq!(e["name"], "");
        assert_eq!(e["canBeRemoved"], false);
        assert!(e.get("id").is_none() && e.get("place_id").is_none());
    }

    #[test]
    fn pagination_is_bounded_and_one_based() {
        assert_eq!(pagination(None, None), (100, 0));
        assert_eq!(pagination(Some(0), Some(-1)), (100, 0));
        assert_eq!(pagination(Some(500), Some(20)), (100, 20));
        assert_eq!(pagination(Some(10), Some(20)), (10, 20));
        let page = PaginatedResponse::one_based(vec![1, 2], 25, 10, 20);
        assert_eq!(
            (page.page, page.pages, page.total, page.limit),
            (3, 3, 25, 10)
        );
        let v = serde_json::to_value(page).unwrap();
        assert_eq!(v["results"], serde_json::json!([1, 2]));
    }

    #[test]
    fn world_ban_status_path_matches_worlds_content_server_client_shape() {
        let filled = WORLD_BAN_STATUS_PATH
            .replace("{world_name}", "foo.eth")
            .replace("{base_parcel}", "0,0")
            .replace("{address}", "0xabc");
        assert_eq!(filled, "/worlds/foo.eth/parcels/0,0/users/0xabc/ban-status");
    }

    #[test]
    fn ban_body_schema_matches_upstream() {
        let ok = |v: Value| BanPlayerBody::schema_validate(&v);
        assert!(ok(serde_json::json!({ "reason": "spam" })).is_ok());
        assert!(
            ok(serde_json::json!({ "reason": "spam", "duration": 1000, "customMessage": "bye" }))
                .is_ok()
        );
        assert!(ok(serde_json::json!({ "reason": "" })).is_err());
        assert!(ok(serde_json::json!({ "reason": "spam", "duration": 0 })).is_err());
        assert!(ok(serde_json::json!({ "reason": "spam", "customMessage": 1 })).is_err());
        assert!(ok(serde_json::json!({ "reason": "spam", "extra": true })).is_err());
        assert!(ok(serde_json::json!([])).is_err());
        assert!(WarnPlayerBody::schema_validate(&serde_json::json!({ "reason": "x" })).is_ok());
        assert!(
            WarnPlayerBody::schema_validate(&serde_json::json!({ "reason": "x", "y": 1 })).is_err()
        );
        assert!(WarnPlayerBody::schema_validate(&serde_json::json!({})).is_err());
    }
}
