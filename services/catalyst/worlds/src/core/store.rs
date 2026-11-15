//! The write statements of the worlds product surface over `worlds.*`
//! (`schemas/20_worlds.sql`), schema-qualified, on the ONE pool. Every world
//! key is `WorldName::key()` (the tables store lowercase names and every
//! statement binds the key, so the PKs serve); the access setting is the
//! three typed columns plus the `access` grants and the communities rows;
//! grants are keyed `(world_name, permission, address)`. Reads that stage 4
//! already ported (`about`, `scenes`, `index`) stay in `worlds::queries`.

use std::collections::HashSet;

use chrono::NaiveDateTime;
use serde_json::value::RawValue;

use super::active_entity::active_entities;
use serde_json::{Value, json};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Postgres, Row};
use uuid::Uuid;

use crate::comms::world_identity::{Parcel, ParcelSet, SceneId, WorldName};
use crate::policy::{AccessType, Deploy, Permission};
use crate::shared::identity::Address;
use catalyst_storage::Storage;

use super::http::ApiError;
use super::settings_policy::{
    DESCRIPTION_MAX_LENGTH, DESCRIPTION_MIN_LENGTH, MAX_CATEGORIES, TITLE_MAX_LENGTH,
    TITLE_MIN_LENGTH, VALID_RATINGS, js_truthy, storable_skybox_time, storable_thumbnail_hash,
    text_len,
};

#[path = "store_access.rs"]
mod access;
#[path = "store_publication.rs"]
mod publication;
#[path = "store_queries.rs"]
mod queries;
#[path = "store_settings.rs"]
mod settings;

pub use access::{
    add_parcels_to_permission, addresses_for_parcel_permission,
    grant_addresses_world_wide_permission, mirror_keeps_local, modify_allow_list_access,
    parcels_for_permission, permission_records, remove_addresses_permission,
    remove_parcels_from_permission, replace_mirrored_access, replace_world_wide_permission,
    store_access_for_owner,
};
#[cfg(test)]
pub(crate) use publication::publish_lock_key;
pub use publication::{
    deploy_scene, deploy_scene_with, effective_base_parcel, ensure_world, entity_timestamp_ms,
    entity_timestamp_number, evict_retired_scenes, newer_scene_deployed, undeploy_scene,
    undeploy_world, world_disabled,
};
pub(super) use publication::{entity_timestamp, entity_timestamp_value};
pub use queries::{
    active_content_keys, admin_block_wallet, admin_list_blocked, admin_list_worlds_page,
    admin_query_access_log, admin_scenes, admin_set_world_blocked, admin_unblock_wallet,
    entities_for_worlds, get_world, insert_access_log, is_world_disabled, list_scenes,
    list_worlds_public, raw_records_exclude, scene_base_parcel, world_is_valid,
};
pub(crate) use settings::scene_settings_from_entity;
pub use settings::{get_world_settings, update_world_settings, world_manifest};

#[cfg(test)]
use queries::collect_content_hashes;

/// The allow-list caps (`ACCESS_MAX_WALLETS`, `ACCESS_MAX_COMMUNITIES`;
/// upstream's defaults 1000 and 50).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessCaps {
    pub wallets: i64,
    pub communities: i64,
}

impl Default for AccessCaps {
    fn default() -> Self {
        Self {
            wallets: 1000,
            communities: 50,
        }
    }
}

/// The world's access setting as the wire carries it (the old JSONB
/// document): the typed columns plus the `access` grants and communities.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AccessSetting {
    #[default]
    Unrestricted,
    /// The bcrypt hash, never the secret.
    SharedSecret {
        secret_hash: String,
    },
    NftOwnership {
        nft: String,
    },
    AllowList {
        wallets: Vec<String>,
        communities: Vec<String>,
    },
}

impl AccessSetting {
    pub fn access_type(&self) -> AccessType {
        match self {
            Self::Unrestricted => AccessType::Unrestricted,
            Self::SharedSecret { .. } => AccessType::SharedSecret,
            Self::NftOwnership { .. } => AccessType::NftOwnership,
            Self::AllowList { .. } => AccessType::AllowList,
        }
    }

    pub fn is_shared_secret(&self) -> bool {
        matches!(self, Self::SharedSecret { .. })
    }

    pub fn to_public_json(&self) -> Value {
        match self {
            Self::Unrestricted => json!({ "type": "unrestricted" }),
            Self::SharedSecret { .. } => json!({ "type": "shared-secret" }),
            Self::NftOwnership { nft } => json!({ "type": "nft-ownership", "nft": nft }),
            Self::AllowList {
                wallets,
                communities,
            } => {
                json!({ "type": "allow-list", "wallets": wallets, "communities": communities })
            }
        }
    }

    fn from_row(r: &PgRow) -> Self {
        match AccessType::parse(&r.get::<String, _>("access_type"))
            .unwrap_or(AccessType::Unrestricted)
        {
            AccessType::Unrestricted => Self::Unrestricted,
            AccessType::SharedSecret => Self::SharedSecret {
                secret_hash: r
                    .get::<Option<String>, _>("access_secret_hash")
                    .unwrap_or_default(),
            },
            AccessType::NftOwnership => Self::NftOwnership {
                nft: r.get::<Option<String>, _>("access_nft").unwrap_or_default(),
            },
            AccessType::AllowList => Self::AllowList {
                wallets: r.get::<Vec<String>, _>("access_wallets"),
                communities: r.get::<Vec<String>, _>("access_communities"),
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorldRecord {
    pub name: String,
    pub owner: Option<Address>,
    pub access: AccessSetting,
    pub blocked_since: Option<NaiveDateTime>,
    pub spawn_coordinates: Option<String>,
    pub skybox_time: Option<i32>,
    pub single_player: bool,
    pub realm_name_override: Option<String>,
    pub preview_wearable_urns: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct PermissionRecord {
    pub permission: String,
    pub address: String,
    pub is_world_wide: bool,
    pub parcel_count: i64,
}

#[derive(Debug, Clone)]
pub struct WorldAdminRow {
    pub name: String,
    pub owner: Option<String>,
    pub access_type: String,
    pub blocked_since: Option<NaiveDateTime>,
    pub spawn_coordinates: Option<String>,
    pub scene_count: i64,
}

#[derive(Debug, Clone)]
pub struct BlockedRow {
    pub wallet: String,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Debug, Clone)]
pub struct AccessLogRow {
    pub id: i64,
    pub world_name: String,
    pub address: String,
    pub action: String,
    pub room: String,
    pub created_at: NaiveDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorldsOrderBy {
    Name,
    LastDeployedAt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderDirection {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Default)]
pub struct WorldsListFilters {
    pub authorized_deployer: Option<String>,
    pub search: Option<String>,
    pub has_deployed_scenes: Option<bool>,
    /// The deny list with each entry's first `.eth` and `.dcl` removed
    /// (upstream `getBannedNames`), compared with the stored name as is.
    pub banned_names: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WorldsListOptions {
    pub limit: i64,
    pub offset: i64,
    pub order_by: WorldsOrderBy,
    pub order_direction: OrderDirection,
}

#[derive(Debug, Clone)]
pub struct WorldInfoRow {
    pub name: String,
    pub owner: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub content_rating: Option<String>,
    pub spawn_coordinates: Option<String>,
    pub skybox_time: Option<i32>,
    pub categories: Option<Vec<String>>,
    pub single_player: bool,
    pub show_in_places: bool,
    pub thumbnail_hash: Option<String>,
    pub last_deployed_at: Option<NaiveDateTime>,
    pub min_x: Option<i32>,
    pub max_x: Option<i32>,
    pub min_y: Option<i32>,
    pub max_y: Option<i32>,
    pub blocked_since: Option<NaiveDateTime>,
    pub deployed_scenes: i64,
}

/// `single_player`/`show_in_places` are NOT NULL in the v2 schema (the old
/// nullable columns rendered their effective defaults on the wire anyway).
#[derive(Debug, Clone, Default)]
pub struct WorldSettingsRow {
    pub title: Option<String>,
    pub description: Option<String>,
    pub content_rating: Option<String>,
    pub spawn_coordinates: Option<String>,
    pub skybox_time: Option<i32>,
    pub categories: Option<Vec<String>>,
    pub single_player: bool,
    pub show_in_places: bool,
    pub thumbnail_hash: Option<String>,
    pub access_type: Option<String>,
    pub realm_name_override: Option<String>,
    pub preview_wearable_urns: Option<Vec<String>>,
    pub settings_version: i64,
}

#[derive(Debug, Clone, Default)]
pub struct WorldSettingsUpdate {
    pub title: Option<String>,
    pub description: Option<String>,
    pub content_rating: Option<String>,
    pub spawn_coordinates: Option<String>,
    pub skybox_time: Option<i32>,
    pub skybox_time_provided: bool,
    pub categories: Option<Vec<String>>,
    pub categories_provided: bool,
    pub single_player: Option<bool>,
    pub show_in_places: Option<bool>,
    pub thumbnail_hash: Option<String>,
    pub realm_name_override: Option<String>,
    pub realm_name_override_provided: bool,
    pub preview_wearable_urns: Option<Vec<String>>,
    pub preview_wearable_urns_provided: bool,
}

#[derive(Debug, Clone)]
pub struct WorldManifest {
    pub parcels: Vec<String>,
    pub spawn_coordinates: Option<String>,
    pub total: i64,
}

/// One edit to an allow-list access setting; wallets arrive lowercased.
#[derive(Debug, Clone, Copy)]
pub enum AllowListEdit<'a> {
    AddWallet(&'a Address),
    RemoveWallet(&'a Address),
    AddCommunity(Uuid),
    RemoveCommunity(Uuid),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowListEditOutcome {
    Applied,
    /// No worlds row, or its access type is not `allow-list`.
    NotAllowList,
    /// The list is at its maximum size.
    CapExceeded,
}

/// One scene deploy as [`deploy_scene`] persists it into `world_scenes`.
pub struct SceneDeployment<'a> {
    pub world: &'a WorldName,
    /// The NAME (or personal-world) owner a `Deploy::Owner` proved; `None`
    /// keeps the stored owner (a scoped grantee never rewrites it).
    pub name_owner: Option<&'a Address>,
    pub entity_id: &'a str,
    pub deployer: &'a Address,
    pub deployment_auth_chain: &'a Value,
    pub entity: &'a Value,
    pub parcels: &'a ParcelSet,
    pub size: i64,
    pub replacement: &'a Deploy,
    /// The upstream this publish is forwarded to (`WORLDS_FORWARD_UPSTREAM_URL`):
    /// its `worlds.forward_outbox` row commits with the scene.
    pub forward_to: Option<&'a str>,
    /// The storage allowance the publish is charged to, re-checked before commit.
    pub quota: Option<&'a super::quota::QuotaCharge>,
}

pub(crate) struct DerivedSceneSettings {
    pub(crate) spawn_coordinates: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) content_rating: Option<String>,
    pub(crate) skybox_time: Option<i32>,
    pub(crate) categories: Option<Vec<String>>,
    pub(crate) single_player: Option<bool>,
    pub(crate) show_in_places: Option<bool>,
    pub(crate) thumbnail_hash: Option<String>,
}

const WORLD_SELECT: &str = r#"SELECT w.name, w.owner, w.access_type, w.access_secret_hash, w.access_nft, w.blocked_since,
              w.spawn_coordinates, w.skybox_time, w.single_player, w.realm_name_override, w.preview_wearable_urns,
              COALESCE((SELECT array_agg(wp.address ORDER BY wp.created_at, wp.address)
                        FROM worlds.world_permissions wp
                        WHERE wp.world_name = w.name AND wp.permission = 'access'), '{}'::text[]) AS access_wallets,
              COALESCE((SELECT array_agg(c.community_id::text ORDER BY c.created_at, c.community_id)
                        FROM worlds.world_access_communities c
                        WHERE c.world_name = w.name), '{}'::text[]) AS access_communities
       FROM worlds.worlds w WHERE w.name = $1"#;

/// What a committed publish reports.
#[derive(Debug)]
pub struct Deployed {
    /// The scene row's creation time (ms).
    pub created_at_ms: i64,
    /// The world's stored owner this publish replaced with a different one.
    pub previous_owner: Option<Address>,
}

/// The partial upload a publish completes: its pending row must still be live (database clock)
/// and its completion receipt commits with the scene.
pub struct PartialCompletion<'a> {
    pub ttl_ms: u64,
    /// The world and parcels the receipt's message names, as the first answer did.
    pub world_name: &'a str,
    pub scene_parcels: &'a [String],
}

/// The refusal of a publish whose partial upload expired (or is gone) before it committed.
pub const PARTIAL_UPLOAD_EXPIRED: &str =
    "This upload expired. Create a new entity with a fresh timestamp.";

/// Which of a mirrored row's settings [`mirror_keeps_local`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetHere {
    /// The access setting (`access_set_here`, `access_set_by`).
    Access,
    /// The settings the mirror copies from `/settings` (`settings_set_here`, `settings_set_by`).
    Settings,
}

const SETTINGS_COLUMNS: &str = "title, description, content_rating, spawn_coordinates, skybox_time, categories, single_player, \
     show_in_places, thumbnail_hash, access_type, realm_name_override, preview_wearable_urns, settings_version";

#[derive(Debug, Clone)]
pub struct AccessLogEntry {
    /// Lowercase.
    pub world_name: String,
    /// Lowercase.
    pub address: String,
    /// `join` | `leave`.
    pub action: &'static str,
    pub room: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entity_with_wc(wc: Value) -> Value {
        json!({ "metadata": { "worldConfiguration": wc } })
    }

    fn entity_with_base(base: Value) -> Value {
        json!({ "metadata": { "scene": { "base": base } } })
    }

    #[test]
    fn effective_base_trusts_declared_base_only_when_a_member() {
        let parcels = vec!["0,0".to_string(), "0,1".to_string()];
        assert_eq!(
            effective_base_parcel(&entity_with_base(json!("0,1")), &parcels),
            Some("0,1".to_string())
        );
        assert_eq!(
            effective_base_parcel(&entity_with_base(json!(" 00 , 01 ")), &parcels),
            Some("0,1".to_string())
        );
        assert_eq!(
            effective_base_parcel(&entity_with_base(json!("9,9")), &parcels),
            Some("0,0".to_string())
        );
        assert_eq!(
            effective_base_parcel(&json!({ "metadata": { "scene": {} } }), &parcels),
            Some("0,0".to_string())
        );
        assert_eq!(
            effective_base_parcel(&entity_with_base(json!("")), &parcels),
            Some("0,0".to_string())
        );
        assert_eq!(
            effective_base_parcel(&entity_with_base(json!("0,0")), &[]),
            None
        );
    }

    #[test]
    fn opt_out_uses_js_truthiness() {
        let opt_out = |v: Value| {
            scene_settings_from_entity(&entity_with_wc(json!({ "placesConfig": { "optOut": v } })))
                .show_in_places
        };
        assert_eq!(opt_out(json!(true)), Some(false));
        assert_eq!(opt_out(json!(1)), Some(false));
        assert_eq!(opt_out(json!("false")), Some(false));
        assert_eq!(opt_out(json!({})), Some(false));
        assert_eq!(opt_out(json!(false)), Some(true));
        assert_eq!(opt_out(json!(0)), Some(true));
        assert_eq!(opt_out(json!(null)), Some(true));
        let undeclared = scene_settings_from_entity(&entity_with_wc(json!({ "placesConfig": {} })));
        assert_eq!(undeclared.show_in_places, None);
    }

    #[test]
    fn text_bounds_are_utf16_code_units() {
        let entity = json!({ "metadata": { "display": {
            "title": "\u{65E5}\u{672C}",
            "description": "\u{30C7}".repeat(400),
        } } });
        let s = scene_settings_from_entity(&entity);
        assert_eq!(s.title, None, "2 UTF-16 units is under the 3-unit minimum");
        assert_eq!(
            s.description.as_deref().map(text_len),
            Some(400),
            "400 UTF-16 units is within the 1000-unit maximum despite 1200 bytes"
        );
    }

    #[test]
    fn skybox_time_shares_the_settings_policy_coercion() {
        let skybox = |v: Value| {
            scene_settings_from_entity(&entity_with_wc(
                json!({ "skyboxConfig": { "fixedTime": v } }),
            ))
            .skybox_time
        };
        assert_eq!(skybox(json!(36000)), Some(36000));
        assert_eq!(skybox(json!(36000.0)), Some(36000));
        assert_eq!(skybox(json!(1.5)), None);
        assert_eq!(skybox(json!(99999999999i64)), None);
    }

    #[test]
    fn content_hashes_fail_closed_on_anything_unreadable() {
        let mut out = HashSet::new();
        assert!(collect_content_hashes("e", None, &mut out).is_ok());
        assert!(collect_content_hashes("e", Some(&Value::Null), &mut out).is_ok());
        assert!(out.is_empty());

        assert!(
            collect_content_hashes(
                "e",
                Some(
                    &json!([{ "file": "a.png", "hash": "h1" }, { "file": "b.png", "hash": "h2" }])
                ),
                &mut out
            )
            .is_ok()
        );
        assert_eq!(out.len(), 2);
        assert!(out.contains("h1") && out.contains("h2"));

        assert!(collect_content_hashes("e", Some(&json!({})), &mut out).is_err());
        assert!(collect_content_hashes("e", Some(&json!("nope")), &mut out).is_err());
        assert!(
            collect_content_hashes("e", Some(&json!([{ "file": "a.png" }])), &mut out).is_err()
        );
        assert!(collect_content_hashes("e", Some(&json!([{ "hash": "" }])), &mut out).is_err());
        assert!(collect_content_hashes("e", Some(&json!([{ "hash": 7 }])), &mut out).is_err());
    }

    #[test]
    fn the_access_setting_renders_the_old_public_document() {
        assert_eq!(
            AccessSetting::Unrestricted.to_public_json(),
            json!({ "type": "unrestricted" })
        );
        assert_eq!(
            AccessSetting::SharedSecret {
                secret_hash: "h".into()
            }
            .to_public_json(),
            json!({ "type": "shared-secret" })
        );
        assert_eq!(
            AccessSetting::NftOwnership {
                nft: "urn:x".into()
            }
            .to_public_json(),
            json!({ "type": "nft-ownership", "nft": "urn:x" })
        );
        assert_eq!(
            AccessSetting::AllowList {
                wallets: vec!["0xabc".into()],
                communities: vec!["c".into()]
            }
            .to_public_json(),
            json!({ "type": "allow-list", "wallets": ["0xabc"], "communities": ["c"] })
        );
        assert_eq!(
            AccessSetting::default().access_type(),
            AccessType::Unrestricted
        );
    }
}
