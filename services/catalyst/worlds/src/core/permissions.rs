//! The world permission routes: the public `GET /world/{name}/permissions`
//! document and the owner-gated grant, parcel and access-list edits. Every
//! owner question is `Policy::is_owner` (personal world, then the name
//! authority, `worlds.owner` only when no reader is configured; an
//! unanswerable authority is a 503, never a silent refusal); the grant
//! writes are `store::*` over the PK-keyed tables.

use std::collections::BTreeMap;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{OriginalUri, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::is_eth_address;
use crate::comms::world_identity::{Parcel, ParcelError, ParcelSet, WorldName};
use crate::content::validation;
use crate::policy::Permission;
use crate::shared::identity::{Address, PermissionDecision};
use crate::shared::signed_fetch::SignedPath;

use super::auth::{PERMISSIONS_METADATA_KEYS, map_auth_error_api, require_verified};
use super::http::{ApiError, QueryPairs, js_parse_int, pagination, query_param};
use super::store::{self, AccessCaps, AccessSetting, AllowListEdit, AllowListEditOutcome};
use super::{Revocation, SharedState};

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "worlds/"))]
pub struct AllowListPermission {
    #[serde(rename = "type")]
    pub kind: String,
    pub wallets: Vec<String>,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "worlds/"))]
pub struct WorldPermissionsBlock {
    pub deployment: AllowListPermission,
    pub streaming: AllowListPermission,
    #[cfg_attr(feature = "ts", ts(type = "unknown"))]
    pub access: Value,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "worlds/"))]
pub struct PermissionSummaryEntry {
    pub permission: String,
    pub world_wide: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(type = "number | null", optional))]
    pub parcel_count: Option<i64>,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "worlds/"))]
pub struct PermissionsResponse {
    pub permissions: WorldPermissionsBlock,
    // Left out when the name has no holder, as upstream's `undefined`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub owner: Option<String>,
    pub summary: BTreeMap<String, Vec<PermissionSummaryEntry>>,
}

pub(super) fn parse_world(raw: &str) -> Result<WorldName, ApiError> {
    WorldName::parse(raw).map_err(|e| ApiError::bad_request(format!("Invalid world name: {e}")))
}

fn parse_address(raw: &str) -> Result<Address, ApiError> {
    Address::parse(raw).map_err(|_| ApiError::bad_request(format!("Invalid address: {raw}.")))
}

/// A path `:address`, as upstream's `EthAddress.validate`: exactly
/// `^0x[a-fA-F0-9]{40}$` (no padding, no `0X`).
fn parse_path_address(raw: &str) -> Result<Address, ApiError> {
    if !is_eth_address(raw) {
        return Err(ApiError::bad_request(format!("Invalid address: {raw}.")));
    }
    parse_address(raw)
}

/// GET /world/{world_name}/permissions
pub async fn get_permissions(
    State(state): State<SharedState>,
    Path(world_name): Path<String>,
) -> Result<Json<PermissionsResponse>, ApiError> {
    let world = parse_world(&world_name)?;
    let record = store::get_world(&state.pool, &world).await?;
    let records = store::permission_records(&state.pool, &world).await?;
    // `getAccessForWorld` reads through `getRawWorldRecords`, whose deny-list
    // filter leaves such a world out: its access reads as the default.
    let denied = store::raw_records_exclude(&state.pool, &state.names, &world).await;
    let access = record
        .as_ref()
        .filter(|_| !denied)
        .map(|w| w.access.clone())
        .unwrap_or_default();
    // The live holder; 503 while the authority cannot answer, as other owner reads.
    let owner = state.policy.current_owner(&world).await?;

    let mut deployment_wallets = Vec::new();
    let mut streaming_wallets = Vec::new();
    let mut summary: BTreeMap<String, Vec<PermissionSummaryEntry>> = BTreeMap::new();
    for r in &records {
        match r.permission.as_str() {
            "deployment" => deployment_wallets.push(r.address.clone()),
            "streaming" => streaming_wallets.push(r.address.clone()),
            _ => {}
        }
        summary
            .entry(r.address.clone())
            .or_default()
            .push(PermissionSummaryEntry {
                permission: r.permission.clone(),
                world_wide: r.is_world_wide,
                parcel_count: if r.is_world_wide {
                    None
                } else {
                    Some(r.parcel_count)
                },
            });
    }

    Ok(Json(PermissionsResponse {
        permissions: WorldPermissionsBlock {
            deployment: AllowListPermission {
                kind: "allow-list".to_string(),
                wallets: deployment_wallets,
            },
            streaming: AllowListPermission {
                kind: "allow-list".to_string(),
                wallets: streaming_wallets,
            },
            access: access.to_public_json(),
        },
        owner: owner.map(|a| a.into_string()),
        summary,
    }))
}

struct OwnerCheck {
    signer: Address,
    /// Whether the `worlds` row already exists, so writers can skip creating it.
    world_found: bool,
}

fn not_owner(world: &WorldName) -> ApiError {
    ApiError::unauthorized(format!(
        "Your wallet does not own \"{}\", you can not set access control lists for it.",
        world.display()
    ))
}

async fn owner_decision(
    state: &SharedState,
    world: &WorldName,
    signer: &Address,
) -> Result<(), ApiError> {
    match state.policy.is_owner(world, signer).await {
        PermissionDecision::Allowed => Ok(()),
        PermissionDecision::Refused(
            crate::shared::identity::AuthorityNotEstablished::RefusedLacksAuthority { .. },
        ) => Err(not_owner(world)),
        PermissionDecision::Refused(undetermined) => Err(undetermined.into()),
    }
}

/// The strict (6.x) signed fetch of the owner-gated routes. Upstream runs it
/// as middleware, before the body, path and owner checks.
async fn verify_signer(
    state: &SharedState,
    headers: &HeaderMap,
    original: &Uri,
    uri: &Uri,
    method: &str,
) -> Result<Address, ApiError> {
    let path = SignedPath::from_uris(original, uri);
    let auth = require_verified(headers, method, &path, &[], state.eip1654.as_deref())
        .await
        .map_err(map_auth_error_api)?;
    Ok(auth.signer)
}

async fn verify_owner(
    state: &SharedState,
    world: &WorldName,
    signer: Address,
) -> Result<OwnerCheck, ApiError> {
    owner_decision(state, world, &signer).await?;
    let world_found = store::get_world(&state.pool, world).await?.is_some();
    Ok(OwnerCheck {
        signer,
        world_found,
    })
}

fn parse_permission(raw: &str) -> Option<Permission> {
    Permission::parse(raw)
}

/// The most addresses one permission edit notifies per direction.
pub const MAX_NOTICES_PER_EDIT: usize = 50;

/// Notifies the addresses the edit in `tx` actually `added` or `removed`
/// (the old permission events, at most [`MAX_NOTICES_PER_EDIT`] each) in the
/// same transaction, then commits.
async fn commit_with_notices(
    state: &SharedState,
    mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
    world: &WorldName,
    permission: Permission,
    added: &[String],
    removed: &[String],
) -> Result<(), ApiError> {
    let now = chrono::Utc::now().naive_utc();
    let capped = |a: &'_ [String]| a.len().min(MAX_NOTICES_PER_EDIT);
    let mut notices = state.notices.permission_changed(
        false,
        world,
        permission,
        &removed[..capped(removed)],
        now,
    );
    notices.extend(state.notices.permission_changed(
        true,
        world,
        permission,
        &added[..capped(added)],
        now,
    ));
    state.notices.send(&mut tx, &notices).await?;
    tx.commit().await?;
    Ok(())
}

/// POST /world/{world_name}/permissions/{permission_name}
pub async fn post_permissions(
    State(state): State<SharedState>,
    Path((world_name, permission_name)): Path<(String, String)>,
    OriginalUri(original): OriginalUri,
    uri: Uri,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let auth = require_verified(
        &headers,
        "post",
        &SignedPath::from_uris(&original, &uri),
        PERMISSIONS_METADATA_KEYS,
        state.eip1654.as_deref(),
    )
    .await
    .map_err(map_auth_error_api)?;
    let world = parse_world(&world_name)?;
    owner_decision(&state, &world, &auth.signer).await?;

    let meta = &auth.metadata;
    match permission_name.as_str() {
        "deployment" => {
            let ty = meta.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if ty != "allow-list" {
                return Err(ApiError::bad_request(
                    "Invalid payload received. Deployment permission needs to be 'allow-list'.",
                ));
            }
            let wallets = metadata_wallets(meta)?;
            let mut tx = state.pool.begin().await?;
            let (added, removed) = store::replace_world_wide_permission(
                &mut *tx,
                &world,
                &auth.signer,
                Permission::Deployment,
                &wallets,
            )
            .await?;
            commit_with_notices(&state, tx, &world, Permission::Deployment, &added, &removed)
                .await?;
        }
        "streaming" => {
            let ty = meta.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if ty != "unrestricted" && ty != "allow-list" {
                return Err(ApiError::bad_request(
                    "Invalid payload received. Streaming permission needs to be either 'unrestricted' or 'allow-list'.",
                ));
            }
            let wallets = if ty == "unrestricted" {
                Vec::new()
            } else {
                metadata_wallets(meta)?
            };
            let mut tx = state.pool.begin().await?;
            let (added, removed) = store::replace_world_wide_permission(
                &mut *tx,
                &world,
                &auth.signer,
                Permission::Streaming,
                &wallets,
            )
            .await?;
            commit_with_notices(&state, tx, &world, Permission::Streaming, &added, &removed)
                .await?;
        }
        "access" => {
            let access = access_from_metadata(meta, state.settings.access_caps)?;
            if let AccessSetting::AllowList { communities, .. } = &access {
                require_community_member(&state, &auth.signer, communities).await?;
            }
            store::store_access_for_owner(&state.pool, &world, &auth.signer, &access).await?;
            state
                .revoked(Revocation::AccessSetting {
                    world: world.clone(),
                })
                .await;
        }
        other => {
            return Err(ApiError::bad_request(format!(
                "Invalid permission name: {other}."
            )));
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

fn metadata_strings(meta: &Value, key: &str) -> Vec<String> {
    meta.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|w| w.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn metadata_wallets(meta: &Value) -> Result<Vec<Address>, ApiError> {
    metadata_strings(meta, "wallets")
        .iter()
        .map(|w| parse_address(w))
        .collect()
}

/// JS `String(value)` of a metadata field, as upstream's template literals
/// print it (`undefined` when absent).
fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => match n.as_f64() {
            Some(f) if n.is_f64() && f.fract() == 0.0 && f.abs() < 1e21 => format!("{f:.0}"),
            _ => n.to_string(),
        },
        // `Array.prototype.join`: null and undefined elements print empty.
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::Null => String::new(),
                other => js_string(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

fn access_from_metadata(meta: &Value, caps: AccessCaps) -> Result<AccessSetting, ApiError> {
    // Upstream switches with `===`, so only a string can name a type.
    let ty = meta.get("type").and_then(|v| v.as_str()).unwrap_or("");
    Ok(match ty {
        "unrestricted" => AccessSetting::Unrestricted,
        "nft-ownership" => {
            let nft = meta.get("nft").and_then(|v| v.as_str()).ok_or_else(|| {
                ApiError::bad_request("For nft ownership there needs to be a valid nft.")
            })?;
            AccessSetting::NftOwnership {
                nft: nft.to_string(),
            }
        }
        "shared-secret" => {
            let secret = meta
                .get("secret")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    ApiError::bad_request("For shared secret there needs to be a valid secret.")
                })?;
            let hash = bcrypt::hash(secret, bcrypt::DEFAULT_COST)
                .map_err(|e| ApiError::internal(format!("hash secret: {e}")))?;
            AccessSetting::SharedSecret { secret_hash: hash }
        }
        "allow-list" => {
            let wallets = metadata_strings(meta, "wallets");
            let communities = metadata_strings(meta, "communities");
            // Upstream checks the communities cap first.
            if communities.len() as i64 > caps.communities {
                return Err(ApiError::bad_request(format!(
                    "Too many communities. Maximum allowed is {}, but {} were provided.",
                    caps.communities,
                    communities.len()
                )));
            }
            if wallets.len() as i64 > caps.wallets {
                return Err(ApiError::bad_request(format!(
                    "Too many wallets in allow-list. Maximum allowed is {}, but {} were provided.",
                    caps.wallets,
                    wallets.len()
                )));
            }
            for w in &wallets {
                parse_address(w)?;
            }
            for c in &communities {
                parse_community(c)?;
            }
            AccessSetting::AllowList {
                wallets,
                communities,
            }
        }
        _ => {
            return Err(ApiError::bad_request(format!(
                "Invalid access type: {}.",
                js_string(meta.get("type"))
            )));
        }
    })
}

/// The signer must belong to every community it lists (the old
/// `UnauthorizedCommunityError`, a 400).
async fn require_community_member(
    state: &SharedState,
    signer: &Address,
    communities: &[String],
) -> Result<(), ApiError> {
    if communities.is_empty() {
        return Ok(());
    }
    let ids = communities
        .iter()
        .map(|c| parse_community(c))
        .collect::<Result<Vec<_>, _>>()?;
    let member = state.policy.member_communities(signer, &ids).await?;
    let missing: Vec<&str> = communities
        .iter()
        .zip(&ids)
        .filter(|(_, id)| !member.contains(id))
        .map(|(raw, _)| raw.as_str())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(ApiError::bad_request(format!(
        "You are not a member of the following communities: {}.",
        missing.join(", ")
    )))
}

/// `worlds.world_access_communities.community_id` is a uuid (the social
/// communities' key); the old service stored any string.
fn parse_community(raw: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(raw.trim()).map_err(|_| ApiError::bad_request("Invalid community id."))
}

fn is_allow_list_permission(p: Permission) -> bool {
    p.supports_parcels()
}

/// PUT /world/{world_name}/permissions/{permission_name}/{address}
pub async fn put_permissions_address(
    State(state): State<SharedState>,
    Path((world_name, permission_name, address)): Path<(String, String, String)>,
    OriginalUri(original): OriginalUri,
    uri: Uri,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let signer = verify_signer(&state, &headers, &original, &uri, "put").await?;
    let address = parse_path_address(&address)?;
    let Some(permission) = parse_permission(&permission_name) else {
        return Err(ApiError::bad_request(format!(
            "Invalid permission name: {permission_name}."
        )));
    };
    let world = parse_world(&world_name)?;
    let owner = verify_owner(&state, &world, signer).await?;
    let ensure_owner = (!owner.world_found).then_some(&owner.signer);

    if is_allow_list_permission(permission) {
        let mut tx = state.pool.begin().await?;
        let added = store::grant_addresses_world_wide_permission(
            &mut *tx,
            &world,
            permission,
            std::slice::from_ref(&address),
            ensure_owner,
        )
        .await?;
        commit_with_notices(&state, tx, &world, permission, &added, &[]).await?;
    } else {
        if let Some(signer) = ensure_owner {
            store::ensure_world(&state.pool, &world, signer).await?;
        }
        apply_allow_list_edit(
            &state,
            &world,
            &owner.signer,
            AllowListEdit::AddWallet(&address),
            |caps| {
                ApiError::bad_request(format!(
                    "Cannot add wallet: allow-list would exceed the maximum of {} wallets.",
                    caps.wallets
                ))
            },
        )
        .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /world/{world_name}/permissions/{permission_name}/{address}
pub async fn delete_permissions_address(
    State(state): State<SharedState>,
    Path((world_name, permission_name, address)): Path<(String, String, String)>,
    OriginalUri(original): OriginalUri,
    uri: Uri,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let signer = verify_signer(&state, &headers, &original, &uri, "delete").await?;
    let address = parse_path_address(&address)?;
    let Some(permission) = parse_permission(&permission_name) else {
        return Err(ApiError::bad_request(format!(
            "Permission '{permission_name}' does not support allow-list. Only 'deployment', 'streaming', and 'access' do."
        )));
    };
    let world = parse_world(&world_name)?;
    owner_decision(&state, &world, &signer).await?;
    if !store::world_is_valid(&state.pool, &state.policy, &state.names, &world).await? {
        return Err(ApiError::not_found(format!(
            "World \"{}\" not found.",
            world.display()
        )));
    }

    if is_allow_list_permission(permission) {
        let mut tx = state.pool.begin().await?;
        let removed = store::remove_addresses_permission(
            &mut *tx,
            &world,
            permission,
            std::slice::from_ref(&address),
        )
        .await?;
        commit_with_notices(&state, tx, &world, permission, &[], &removed).await?;
    } else {
        apply_allow_list_edit(
            &state,
            &world,
            &signer,
            AllowListEdit::RemoveWallet(&address),
            |_| ApiError::internal("unreachable: removals have no cap"),
        )
        .await?;
        // Kicks only if nothing else admits it (e.g. a listed community).
        state.revoke_unless_admitted(&world, &address).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

const MAX_PARCELS_PER_REQUEST: usize = 500;
const PARCEL_PATTERN: &str = "^-?\\d+,-?\\d+$";

/// A refusal of the parcel routes: the worlds envelope, or the
/// `@dcl/schema-validator-component` body upstream's validator middleware
/// answers in its own shape (`{ok: false, message[, data]}`).
#[derive(Debug)]
pub enum ParcelsRefusal {
    Api(ApiError),
    Validator { status: StatusCode, body: Value },
}

impl From<ApiError> for ParcelsRefusal {
    fn from(e: ApiError) -> Self {
        Self::Api(e)
    }
}

impl From<sqlx::Error> for ParcelsRefusal {
    fn from(e: sqlx::Error) -> Self {
        Self::Api(e.into())
    }
}

impl IntoResponse for ParcelsRefusal {
    fn into_response(self) -> Response {
        match self {
            Self::Api(e) => e.into_response(),
            Self::Validator { status, body } => (
                status,
                [(header::CONTENT_TYPE, "application/json")],
                validation::ordered_refusal_json(&body),
            )
                .into_response(),
        }
    }
}

/// Upstream's `withSchemaValidatorMiddleware(permissionParcelsSchema)`: a
/// JSON content type (415), a JSON body (400 with the parse error), then
/// exactly `{parcels}` with 1-500 `x,y` strings (400 `Invalid JSON body` and
/// AJV's first error).
fn parcels_body(headers: &HeaderMap, body: &[u8]) -> Result<ParcelSet, ParcelsRefusal> {
    let refuse = |status: StatusCode, body: Value| ParcelsRefusal::Validator { status, body };
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    if !crate::content::validation::is_json_content_type(content_type) {
        return Err(refuse(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            json!({ "ok": false, "message": "Content-Type must be application/json" }),
        ));
    }
    let value: Value = serde_json::from_slice(body).map_err(|e| {
        refuse(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "message": validation::json_parse_message(body, &e) }),
        )
    })?;
    let keys = validation::object_keys_in_order(&value, body);
    if let Some(error) = parcels_schema_error(&value, &keys) {
        return Err(refuse(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "message": "Invalid JSON body", "data": [error] }),
        ));
    }
    let raw: Vec<&str> = value["parcels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    // The pattern passed; only a coordinate past i32 is left.
    ParcelSet::parse_strict(&raw)
        .map_err(|e| ApiError::bad_request(format!("Invalid parcels: {e}")).into())
}

/// The first error AJV 8 (`allErrors: false`) reports for
/// `permissionParcelsSchema`: `{parcels}` with 1-500 strings matching
/// [`PARCEL_PATTERN`].
fn parcels_schema_error(body: &Value, keys: &[String]) -> Option<Value> {
    validation::array_property_error(body, keys, "", "parcels", MAX_PARCELS_PER_REQUEST, |item| {
        match item.as_str() {
            None => Some(validation::ItemError::not_a_string()),
            // `Parcel::parse_strict`'s shape is the pattern (JS `$`: the end only).
            Some(raw) if matches!(Parcel::parse_strict(raw), Err(ParcelError::NotStrict(_))) => {
                Some(validation::ItemError {
                    keyword: "pattern",
                    params: json!({ "pattern": PARCEL_PATTERN }),
                    message: format!("must match pattern \"{PARCEL_PATTERN}\""),
                })
            }
            Some(_) => None,
        }
    })
}

fn validate_address_and_allow_list(
    address: &str,
    permission_name: &str,
) -> Result<(Address, Permission), ApiError> {
    let address = parse_path_address(address)?;
    match parse_permission(permission_name) {
        Some(p) if is_allow_list_permission(p) => Ok((address, p)),
        _ => Err(ApiError::bad_request(format!(
            "Permission '{permission_name}' does not support allow-list. Only 'deployment' and 'streaming' do."
        ))),
    }
}

/// POST /world/{world_name}/permissions/{permission_name}/address/{address}/parcels
pub async fn post_permission_parcels(
    State(state): State<SharedState>,
    Path((world_name, permission_name, address)): Path<(String, String, String)>,
    OriginalUri(original): OriginalUri,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ParcelsRefusal> {
    let signer = verify_signer(&state, &headers, &original, &uri, "post").await?;
    let parcels = parcels_body(&headers, &body)?;
    let (address, permission) = validate_address_and_allow_list(&address, &permission_name)?;
    let world = parse_world(&world_name)?;
    verify_owner(&state, &world, signer).await?;
    let mut tx = state.pool.begin().await?;
    let created =
        store::add_parcels_to_permission(&mut *tx, &world, permission, &address, &parcels).await?;
    let added: &[String] = if created {
        &[address.as_str().to_string()]
    } else {
        &[]
    };
    commit_with_notices(&state, tx, &world, permission, added, &[]).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /world/{world_name}/permissions/{permission_name}/address/{address}/parcels
pub async fn delete_permission_parcels(
    State(state): State<SharedState>,
    Path((world_name, permission_name, address)): Path<(String, String, String)>,
    OriginalUri(original): OriginalUri,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ParcelsRefusal> {
    let signer = verify_signer(&state, &headers, &original, &uri, "delete").await?;
    let parcels = parcels_body(&headers, &body)?;
    let raw_address = address;
    let (address, permission) = validate_address_and_allow_list(&raw_address, &permission_name)?;
    let world = parse_world(&world_name)?;
    verify_owner(&state, &world, signer).await?;
    let mut tx = state.pool.begin().await?;
    let removal =
        store::remove_parcels_from_permission(&mut *tx, &world, permission, &address, &parcels)
            .await?;
    let Some(dropped) = removal else {
        return Err(ApiError::bad_request(format!(
            "Permission not found. Address {raw_address} does not have {permission_name} permission for world {}.",
            world.display()
        ))
        .into());
    };
    // Removing the last parcel drops the grant, so the address hears it was revoked.
    let removed: &[String] = if dropped {
        &[address.as_str().to_string()]
    } else {
        &[]
    };
    commit_with_notices(&state, tx, &world, permission, &[], removed).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn bounding_box(q: &QueryPairs) -> Result<Option<(i32, i32, i32, i32)>, ApiError> {
    let params = ["x1", "y1", "x2", "y2"].map(|k| query_param(q, k));
    if params.iter().all(Option::is_none) {
        return Ok(None);
    }
    let [Some(x1), Some(y1), Some(x2), Some(y2)] = params else {
        return Err(ApiError::bad_request(
            "Bounding box requires all four parameters: x1, y1, x2, y2.",
        ));
    };
    let coordinate = |raw: &str| {
        js_parse_int(raw)
            .map(|n| n.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
            .ok_or_else(|| ApiError::bad_request("Bounding box parameters must be valid integers."))
    };
    Ok(Some((
        coordinate(x1)?,
        coordinate(y1)?,
        coordinate(x2)?,
        coordinate(y2)?,
    )))
}

/// GET /world/{world_name}/permissions/{permission_name}/address/{address}/parcels
pub async fn get_allowed_parcels_for_permission(
    State(state): State<SharedState>,
    Path((world_name, permission_name, address)): Path<(String, String, String)>,
    Query(q): Query<QueryPairs>,
) -> Result<Json<Value>, ApiError> {
    let raw_address = address;
    let (address, permission) = validate_address_and_allow_list(&raw_address, &permission_name)?;
    let world = parse_world(&world_name)?;
    let (limit, offset) = pagination(&q);
    let bbox = bounding_box(&q)?;
    let (total, parcels) = store::parcels_for_permission(
        &state.pool,
        &world,
        permission,
        &address,
        limit,
        offset,
        bbox,
    )
    .await?
    .ok_or_else(|| {
        ApiError::not_found(format!(
            "Address {raw_address} does not have {permission_name} permission for world {}.",
            world.display()
        ))
    })?;
    Ok(Json(json!({ "total": total, "parcels": parcels })))
}

/// POST /world/{world_name}/permissions/{permission_name}/parcels
pub async fn get_addresses_for_parcel_permission(
    State(state): State<SharedState>,
    Path((world_name, permission_name)): Path<(String, String)>,
    Query(q): Query<QueryPairs>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ParcelsRefusal> {
    let parcels = parcels_body(&headers, &body)?;
    let permission = match parse_permission(&permission_name) {
        Some(p) if is_allow_list_permission(p) => p,
        _ => {
            return Err(ApiError::bad_request(format!(
                "Permission '{permission_name}' does not support allow-list. Only 'deployment' and 'streaming' do."
            ))
            .into());
        }
    };
    let world = parse_world(&world_name)?;
    let (limit, offset) = pagination(&q);
    let (total, addresses) = store::addresses_for_parcel_permission(
        &state.pool,
        &world,
        permission,
        &parcels,
        limit,
        offset,
    )
    .await?;
    Ok(Json(json!({ "total": total, "addresses": addresses })))
}

/// PUT and DELETE /world/{world_name}/permissions/{permission_name}/parcels:
/// upstream's router falls through to `:permission_name/:address` with the
/// address `parcels`; matchit picks the static segment, so these hand it on.
pub async fn put_permissions_parcels_address(
    state: State<SharedState>,
    Path((world_name, permission_name)): Path<(String, String)>,
    original: OriginalUri,
    uri: Uri,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let path = Path((world_name, permission_name, "parcels".to_string()));
    put_permissions_address(state, path, original, uri, headers).await
}

pub async fn delete_permissions_parcels_address(
    state: State<SharedState>,
    Path((world_name, permission_name)): Path<(String, String)>,
    original: OriginalUri,
    uri: Uri,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let path = Path((world_name, permission_name, "parcels".to_string()));
    delete_permissions_address(state, path, original, uri, headers).await
}

/// PUT /world/{world_name}/permissions/access/communities/{communityId}
pub async fn put_permissions_access_community(
    State(state): State<SharedState>,
    Path((world_name, community_id)): Path<(String, String)>,
    OriginalUri(original): OriginalUri,
    uri: Uri,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let signer = verify_signer(&state, &headers, &original, &uri, "put").await?;
    let community = parse_community(&community_id)?;
    let world = parse_world(&world_name)?;
    let owner = verify_owner(&state, &world, signer).await?;
    require_community_member(&state, &owner.signer, &[community_id]).await?;
    apply_allow_list_edit(
        &state,
        &world,
        &owner.signer,
        AllowListEdit::AddCommunity(community),
        |caps| {
            ApiError::bad_request(format!(
                "Too many communities. Maximum allowed is {}, cannot add more.",
                caps.communities
            ))
        },
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /world/{world_name}/permissions/access/communities/{communityId}
pub async fn delete_permissions_access_community(
    State(state): State<SharedState>,
    Path((world_name, community_id)): Path<(String, String)>,
    OriginalUri(original): OriginalUri,
    uri: Uri,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let signer = verify_signer(&state, &headers, &original, &uri, "delete").await?;
    let community = parse_community(&community_id)?;
    let world = parse_world(&world_name)?;
    let owner = verify_owner(&state, &world, signer).await?;
    apply_allow_list_edit(
        &state,
        &world,
        &owner.signer,
        AllowListEdit::RemoveCommunity(community),
        |_| ApiError::internal("unreachable: removals have no cap"),
    )
    .await?;
    state
        .revoked(Revocation::AccessSetting {
            world: world.clone(),
        })
        .await;
    Ok(StatusCode::NO_CONTENT)
}

async fn apply_allow_list_edit(
    state: &SharedState,
    world: &WorldName,
    owner: &Address,
    edit: AllowListEdit<'_>,
    cap_error: impl FnOnce(AccessCaps) -> ApiError,
) -> Result<(), ApiError> {
    let caps = state.settings.access_caps;
    match store::modify_allow_list_access(&state.pool, world, owner, caps, edit).await? {
        AllowListEditOutcome::Applied => Ok(()),
        AllowListEditOutcome::NotAllowList => Err(ApiError::bad_request(format!(
            "World \"{}\" does not have allow-list access type. Cannot add or remove wallets.",
            world.display()
        ))),
        AllowListEditOutcome::CapExceeded => Err(cap_error(caps)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn access(meta: &Value) -> Result<AccessSetting, ApiError> {
        access_from_metadata(meta, AccessCaps::default())
    }

    #[test]
    fn access_metadata_parses_the_old_document_shapes() {
        assert_eq!(
            access(&json!({ "type": "unrestricted" })).unwrap(),
            AccessSetting::Unrestricted
        );
        assert_eq!(
            access(&json!({ "type": "nft-ownership", "nft": "urn:x" })).unwrap(),
            AccessSetting::NftOwnership {
                nft: "urn:x".into()
            }
        );
        assert_eq!(
            access(&json!({ "type": "nft-ownership" }))
                .unwrap_err()
                .status(),
            400
        );
        assert_eq!(
            access(&json!({ "type": "shared-secret", "secret": "" }))
                .unwrap_err()
                .status(),
            400
        );
        assert!(matches!(
            access(&json!({ "type": "shared-secret", "secret": "MySecret" })).unwrap(),
            AccessSetting::SharedSecret { secret_hash } if bcrypt::verify("MySecret", &secret_hash).unwrap()
        ));
        let id = Uuid::new_v4().to_string();
        assert_eq!(
            access(&json!({ "type": "allow-list", "wallets": ["0xAbC0000000000000000000000000000000000001"], "communities": [id] })).unwrap(),
            AccessSetting::AllowList { wallets: vec!["0xAbC0000000000000000000000000000000000001".into()], communities: vec![id.clone()] }
        );
        assert_eq!(
            access(&json!({ "type": "allow-list", "wallets": ["zzz"] }))
                .unwrap_err()
                .status(),
            400
        );
        assert_eq!(
            access(&json!({ "type": "allow-list", "communities": ["not-a-uuid"] }))
                .unwrap_err()
                .status(),
            400
        );
        assert_eq!(
            access(&json!({ "type": "bogus" })).unwrap_err().status(),
            400
        );
        let many: Vec<String> = (0..1001).map(|i| format!("0x{i:040x}")).collect();
        assert!(
            access(&json!({ "type": "allow-list", "wallets": many }))
                .unwrap_err()
                .public_message()
                .contains("Too many wallets")
        );
    }

    /// `access/component.ts:61-71`: communities are checked first, against
    /// the configured caps.
    #[test]
    fn allow_list_caps_come_from_settings_communities_first() {
        let caps = AccessCaps {
            wallets: 1,
            communities: 1,
        };
        let wallets = [
            "0x0000000000000000000000000000000000000001",
            "0x0000000000000000000000000000000000000002",
        ];
        let communities = [Uuid::new_v4().to_string(), Uuid::new_v4().to_string()];
        let both = json!({ "type": "allow-list", "wallets": wallets, "communities": communities });
        assert_eq!(
            access_from_metadata(&both, caps)
                .unwrap_err()
                .public_message(),
            "Too many communities. Maximum allowed is 1, but 2 were provided."
        );
        let wallets_only = json!({ "type": "allow-list", "wallets": wallets });
        assert_eq!(
            access_from_metadata(&wallets_only, caps)
                .unwrap_err()
                .public_message(),
            "Too many wallets in allow-list. Maximum allowed is 1, but 2 were provided."
        );
        assert!(access(&both).is_ok());
    }

    /// `access/component.ts:112-114` prints `type` through a template literal
    /// (`post-permissions.spec.ts`: "and no type is provided").
    #[test]
    fn an_invalid_access_type_prints_as_javascript_does() {
        for (meta, shown) in [
            (json!({}), "undefined"),
            (json!({ "type": null }), "null"),
            (json!({ "type": "invalid" }), "invalid"),
            (json!({ "type": 7 }), "7"),
            (json!({ "type": 1.5 }), "1.5"),
            (json!({ "type": 2.0 }), "2"),
            (json!({ "type": true }), "true"),
            (json!({ "type": ["allow-list", null, 1] }), "allow-list,,1"),
            (json!({ "type": { "a": 1 } }), "[object Object]"),
        ] {
            assert_eq!(
                access(&meta).unwrap_err().public_message(),
                format!("Invalid access type: {shown}."),
                "{meta}"
            );
        }
    }

    #[test]
    fn route_parameter_validation_matches_the_old_messages() {
        assert_eq!(
            validate_address_and_allow_list("zzz", "deployment")
                .unwrap_err()
                .public_message(),
            "Invalid address: zzz."
        );
        assert!(
            validate_address_and_allow_list("0x0000000000000000000000000000000000000001", "access")
                .unwrap_err()
                .public_message()
                .contains("Only 'deployment' and 'streaming' do.")
        );
        assert!(
            validate_address_and_allow_list(
                "0x0000000000000000000000000000000000000001",
                "streaming"
            )
            .is_ok()
        );
        assert!(parse_community(" ").is_err());
        // `EthAddress.validate`: no padding, no `0X`.
        for raw in [
            " 0x0000000000000000000000000000000000000001",
            "0X0000000000000000000000000000000000000001",
            "0x00000000000000000000000000000000000000001",
        ] {
            assert_eq!(
                parse_path_address(raw).unwrap_err().public_message(),
                format!("Invalid address: {raw}.")
            );
        }
        assert!(parse_path_address("0xAbC0000000000000000000000000000000000001").is_ok());
    }

    fn pairs(raw: &[(&str, &str)]) -> QueryPairs {
        raw.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn bounding_box_parses_like_parse_int() {
        assert_eq!(bounding_box(&pairs(&[])).unwrap(), None);
        let full = |x1| pairs(&[("x1", x1), ("y1", "0"), ("x2", "1"), ("y2", "-1")]);
        assert_eq!(bounding_box(&full("1.5")).unwrap(), Some((1, 0, 1, -1)));
        assert_eq!(
            bounding_box(&full("abc")).unwrap_err().public_message(),
            "Bounding box parameters must be valid integers."
        );
        assert_eq!(
            bounding_box(&full("")).unwrap_err().public_message(),
            "Bounding box parameters must be valid integers."
        );
        assert_eq!(
            bounding_box(&pairs(&[("x1", "0"), ("y1", "0")]))
                .unwrap_err()
                .public_message(),
            "Bounding box requires all four parameters: x1, y1, x2, y2."
        );
    }

    fn json_headers(content_type: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, content_type.parse().unwrap());
        headers
    }

    fn refusal(content_type: &str, body: &[u8]) -> (StatusCode, String) {
        match parcels_body(&json_headers(content_type), body) {
            Err(ParcelsRefusal::Validator { status, body }) => {
                (status, validation::ordered_refusal_json(&body))
            }
            other => panic!("not a validator refusal: {other:?}"),
        }
    }

    #[test]
    fn parcels_body_matches_the_upstream_schema() {
        let json = "application/json";
        assert_eq!(
            parcels_body(
                &json_headers(json),
                br#"{"parcels":["0,0","-1,-20","00,01"]}"#
            )
            .unwrap()
            .canonical(),
            vec!["-1,-20".to_string(), "0,0".into(), "0,1".into()]
        );
        let max: Vec<String> = (0..500).map(|i| format!("{i},0")).collect();
        let max = json!({ "parcels": max }).to_string();
        assert!(parcels_body(&json_headers(json), max.as_bytes()).is_ok());
        assert!(
            parcels_body(
                &json_headers("application/vnd.dcl+json; charset=utf-8"),
                max.as_bytes()
            )
            .is_ok()
        );
        let error = |instance: &str, schema: &str, keyword: &str, params: &str, message: &str| {
            format!(
                r##"{{"ok":false,"message":"Invalid JSON body","data":[{{"instancePath":"{instance}","schemaPath":"#/{schema}","keyword":"{keyword}","params":{params},"message":"{message}"}}]}}"##
            )
        };
        let over: Vec<String> = (0..501).map(|_| "a".to_string()).collect();
        let over = json!({ "parcels": over }).to_string();
        // Each body's answer as AJV 8 gives it for `permissionParcelsSchema`.
        let pattern = r#"must match pattern \"^-?\\d+,-?\\d+$\""#;
        let pattern_params = r#"{"pattern":"^-?\\d+,-?\\d+$"}"#;
        for (body, expected) in [
            (
                "[]".to_string(),
                error("", "type", "type", r#"{"type":"object"}"#, "must be object"),
            ),
            (
                "{}".into(),
                error(
                    "",
                    "required",
                    "required",
                    r#"{"missingProperty":"parcels"}"#,
                    "must have required property 'parcels'",
                ),
            ),
            (
                r#"{"a":1,"parcels":["0,0"]}"#.into(),
                error(
                    "",
                    "additionalProperties",
                    "additionalProperties",
                    r#"{"additionalProperty":"a"}"#,
                    "must NOT have additional properties",
                ),
            ),
            (
                r#"{"parcels":["0,0"],"b":1,"a":2}"#.into(),
                error(
                    "",
                    "additionalProperties",
                    "additionalProperties",
                    r#"{"additionalProperty":"b"}"#,
                    "must NOT have additional properties",
                ),
            ),
            (
                r#"{"parcels":"0,0"}"#.into(),
                error(
                    "/parcels",
                    "properties/parcels/type",
                    "type",
                    r#"{"type":"array"}"#,
                    "must be array",
                ),
            ),
            (
                r#"{"parcels":[]}"#.into(),
                error(
                    "/parcels",
                    "properties/parcels/minItems",
                    "minItems",
                    r#"{"limit":1}"#,
                    "must NOT have fewer than 1 items",
                ),
            ),
            (
                over,
                error(
                    "/parcels",
                    "properties/parcels/maxItems",
                    "maxItems",
                    r#"{"limit":500}"#,
                    "must NOT have more than 500 items",
                ),
            ),
            (
                r#"{"parcels":[1,"0,0"]}"#.into(),
                error(
                    "/parcels/0",
                    "properties/parcels/items/type",
                    "type",
                    r#"{"type":"string"}"#,
                    "must be string",
                ),
            ),
            (
                r#"{"parcels":["0,0","a"]}"#.into(),
                error(
                    "/parcels/1",
                    "properties/parcels/items/pattern",
                    "pattern",
                    pattern_params,
                    pattern,
                ),
            ),
            (
                r#"{"parcels":["0,0\n"]}"#.into(),
                error(
                    "/parcels/0",
                    "properties/parcels/items/pattern",
                    "pattern",
                    pattern_params,
                    pattern,
                ),
            ),
            (
                r#"{"parcels":["+1,0"]}"#.into(),
                error(
                    "/parcels/0",
                    "properties/parcels/items/pattern",
                    "pattern",
                    pattern_params,
                    pattern,
                ),
            ),
        ] {
            assert_eq!(
                refusal(json, body.as_bytes()),
                (StatusCode::BAD_REQUEST, expected),
                "{body}"
            );
        }
        assert_eq!(
            refusal("text/plain", br#"{"parcels":["0,0"]}"#),
            (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                r#"{"ok":false,"message":"Content-Type must be application/json"}"#.to_string()
            )
        );
        // An unparsable body answers `JSON.parse`'s V8 message.
        for (body, message) in [
            (
                &b"{"[..],
                "Expected property name or '}' in JSON at position 1 (line 1 column 2)",
            ),
            (b"", "Unexpected end of JSON input"),
            (b"oops", "Unexpected token 'o', \"oops\" is not valid JSON"),
        ] {
            assert_eq!(
                refusal(json, body),
                (
                    StatusCode::BAD_REQUEST,
                    format!(r#"{{"ok":false,"message":{}}}"#, Value::from(message))
                ),
                "{body:?}"
            );
        }
        // AJV passes it (pattern only); only i32 storage refuses it.
        assert!(matches!(
            parcels_body(&json_headers(json), br#"{"parcels":["99999999999,0"]}"#),
            Err(ParcelsRefusal::Api(e)) if e.public_message().starts_with("Invalid parcels:")
        ));
    }
}
