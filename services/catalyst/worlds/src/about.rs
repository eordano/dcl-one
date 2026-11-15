use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;
use serde_json::Value;

use super::SharedState;
use super::queries::{self, WorldScene};
use crate::core::http::ApiError;
use crate::core::settings_policy::js_truthy;
use crate::index::content_file_hash;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "worlds/"))]
pub struct MinimapConfig {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub data_image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub estate_image: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "worlds/"))]
pub struct SkyboxConfig {
    /// Absent when neither the world row nor the scene fixes a time; the
    /// number as stored or deployed (an integer stays an integer).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional, type = "number"))]
    pub fixed_hour: Option<serde_json::Number>,
    pub textures: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "worlds/"))]
pub struct AboutMap {
    pub minimap_enabled: bool,
    #[cfg_attr(feature = "ts", ts(type = "unknown[]"))]
    pub sizes: Vec<Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "worlds/"))]
pub struct AboutConfigurations {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub network_id: i64,
    pub global_scenes_urn: Vec<String>,
    pub scenes_urn: Vec<String>,
    pub minimap: MinimapConfig,
    pub skybox: SkyboxConfig,
    pub realm_name: String,
    pub map: AboutMap,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "worlds/"))]
pub struct AboutContentStatus {
    pub synchronization_status: String,
    pub healthy: bool,
    pub public_url: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "worlds/"))]
pub struct AboutServiceStatus {
    pub healthy: bool,
    pub public_url: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "worlds/"))]
pub struct AboutComms {
    pub healthy: bool,
    pub protocol: String,
    pub adapter: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "worlds/"))]
pub struct AboutResponse {
    pub healthy: bool,
    pub accepting_users: bool,
    pub spawn_coordinates: Option<String>,
    pub configurations: AboutConfigurations,
    pub content: AboutContentStatus,
    pub lambdas: AboutServiceStatus,
    pub comms: AboutComms,
}

/// Whether this node's own content (and lambdas, same process) can serve a
/// world's visitors: the shared sync state, read locally. `None` (an external
/// `CONTENT_URL`, or tests) has no local signal and reports healthy.
#[derive(Clone, Default)]
pub struct ContentReadiness(Option<Arc<crate::content::SyncStatus>>);

impl ContentReadiness {
    pub fn local(sync: Arc<crate::content::SyncStatus>) -> Self {
        Self(Some(sync))
    }

    /// Unhealthy only when the content sync failed; a bootstrapping node or one with sync off still serves worlds.
    pub fn content_healthy(&self) -> bool {
        self.0.as_ref().is_none_or(|s| s.state_name() != "Failed")
    }
}

impl std::fmt::Debug for ContentReadiness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ContentReadiness")
            .field(&self.0.as_ref().map(|s| s.state_name()))
            .finish()
    }
}

/// GET /world/{world_name}/about, answering in the worlds envelope.
pub async fn get_about(
    State(state): State<SharedState>,
    Path(world_name): Path<String>,
) -> Result<Json<AboutResponse>, ApiError> {
    if !state
        .names
        .check_name_deny_list(&state.pool, &world_name)
        .await
    {
        return Err(ApiError::not_found(format!(
            "World \"{world_name}\" has no scene deployed."
        )));
    }
    let read = |e: crate::content::errors::AppError| ApiError::internal(e.to_string());
    let mut about = queries::world_about(&state.pool, &world_name)
        .await
        .map_err(read)?;
    if about.scenes.is_empty() && state.fill_world(&world_name).await {
        about = queries::world_about(&state.pool, &world_name)
            .await
            .map_err(read)?;
    }
    // The live NAME holder's quota block, not the stored owner's.
    if let (Some(policy), Some(world)) = (state.policy(), about.world.as_mut())
        && let Ok(name) = crate::comms::world_identity::WorldName::parse(&world.name)
    {
        world.quota_blocked_since = policy
            .quota_blocked_since(&name)
            .await
            .map_err(|e| ApiError::internal(format!("quota block: {e}")))?;
    }
    Ok(Json(build_about(&state.settings, &world_name, about)?))
}

pub fn build_about(
    cfg: &super::Settings,
    world_name: &str,
    about: queries::WorldAbout,
) -> Result<AboutResponse, ApiError> {
    let world = &about.world;
    let scenes = &about.scenes;

    if scenes.is_empty() {
        return Err(ApiError::not_found(format!(
            "World \"{world_name}\" has no scenes deployed."
        )));
    }
    let (disabled, quota) = world
        .as_ref()
        .map_or((None, None), |w| (w.blocked_since, w.quota_blocked_since));
    if let Some(since) = disabled.or(quota)
        && crate::comms::access::world_blocked(disabled, quota, chrono::Utc::now().naive_utc())
    {
        let cause = if disabled.is_some() {
            "by an operator"
        } else {
            "as it exceeded its allowed storage space"
        };
        // Upstream interpolates a `Date`: `Date.prototype.toString()` in UTC.
        return Err(ApiError::unauthorized(format!(
            "World \"{world_name}\" has been blocked since {} {cause}.",
            since.format("%a %b %d %Y %H:%M:%S GMT+0000 (Coordinated Universal Time)")
        )));
    }

    let base_url = &cfg.public_url;
    let contents_url = cfg.content_server_url();
    let scenes_urn: Vec<String> = scenes
        .iter()
        .take(1)
        .map(|s| {
            format!(
                "urn:decentraland:entity:{}?=&baseUrl={contents_url}/contents/",
                s.entity_id
            )
        })
        .collect();

    let primary = &scenes[0];
    let mut rt = RuntimeMeta::from_scene(world_name, primary);
    rt.name = resolve_realm_name(
        &rt.name,
        world
            .as_ref()
            .and_then(|w| w.realm_name_override.as_deref()),
        cfg.realm_name_strip_ens,
        is_locally_published(scenes),
    );
    if let Some(w) = world {
        if let Some(t) = w.skybox_time {
            rt.skybox_fixed_time = Some(t.into());
        }
        if w.single_player {
            rt.fixed_adapter = Some("offline:offline".to_string());
        }
    }

    let spawn_coordinates = world.as_ref().and_then(|w| w.spawn_coordinates.clone());

    let url_for_file = |filename: &Option<String>, default_image: &str| -> String {
        match filename {
            Some(f) => format!("{contents_url}/contents/{f}"),
            None => default_image.to_string(),
        }
    };
    let data_image = (rt.minimap_visible || rt.minimap_data_image.is_some())
        .then(|| url_for_file(&rt.minimap_data_image, &cfg.map_parcel_view_url));
    let estate_image = (rt.minimap_visible || rt.minimap_estate_image.is_some())
        .then(|| url_for_file(&rt.minimap_estate_image, &cfg.map_estate_view_url));

    let adapter = resolve_fixed_adapter(
        world_name,
        rt.fixed_adapter.as_deref(),
        base_url,
        cfg.comms_online && cfg.sfu.is_alive(),
    );

    // Lambdas run in this process: only content's sync state can make the node unhealthy.
    let content_healthy = cfg.content_readiness.content_healthy();
    Ok(AboutResponse {
        healthy: content_healthy,
        accepting_users: content_healthy,
        spawn_coordinates,
        configurations: AboutConfigurations {
            network_id: cfg.network_id,
            global_scenes_urn: cfg.global_scenes_urn.clone(),
            scenes_urn,
            minimap: MinimapConfig {
                enabled: rt.minimap_visible,
                data_image,
                estate_image,
            },
            skybox: SkyboxConfig {
                fixed_hour: rt.skybox_fixed_time,
                // An unresolved texture links as upstream's
                // `urlForFile(undefined)`: `""`.
                textures: rt
                    .skybox_textures
                    .iter()
                    .map(|t| {
                        t.as_ref()
                            .map_or_else(String::new, |t| format!("{contents_url}/contents/{t}"))
                    })
                    .collect(),
            },
            realm_name: rt.name,
            map: AboutMap {
                minimap_enabled: false,
                sizes: Vec::new(),
            },
        },
        content: AboutContentStatus {
            synchronization_status: "Syncing".to_string(),
            healthy: content_healthy,
            public_url: cfg.content_public_url.clone(),
        },
        lambdas: AboutServiceStatus {
            healthy: true,
            public_url: cfg.lambdas_public_url.clone(),
        },
        comms: AboutComms {
            healthy: true,
            protocol: "v3".to_string(),
            adapter,
        },
    })
}

const ENS_WORLD_SUFFIX: &str = ".dcl.eth";

/// Whether an owner deployed to this node, as opposed to the mirror copying
/// someone else's world here. A mirror keeps the ENS realm name pointing
/// clients at the original registry; a local publish is authoritative and
/// must not lose to it.
fn is_locally_published(scenes: &[WorldScene]) -> bool {
    scenes.iter().any(WorldScene::is_local)
}

fn strip_ens_suffix(name: &str) -> &str {
    let cut = name.len().saturating_sub(ENS_WORLD_SUFFIX.len());
    match name[cut..].eq_ignore_ascii_case(ENS_WORLD_SUFFIX) && cut > 0 {
        true => &name[..cut],
        false => name,
    }
}

/// An explicit override always wins: it is the only value here a republish
/// cannot overwrite, which is why the column exists.
fn resolve_realm_name(
    derived: &str,
    override_name: Option<&str>,
    strip_ens: bool,
    locally_published: bool,
) -> String {
    if let Some(name) = override_name.map(str::trim).filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    match strip_ens && locally_published {
        true => strip_ens_suffix(derived).to_string(),
        false => derived.to_string(),
    }
}

fn resolve_fixed_adapter(
    world_name: &str,
    fixed_adapter: Option<&str>,
    base_url: &str,
    comms_online: bool,
) -> String {
    if fixed_adapter == Some("offline:offline") || !comms_online {
        return "fixed-adapter:offline:offline".to_string();
    }
    format!(
        "fixed-adapter:signed-login:{base_url}/worlds/{}/comms",
        world_name.to_lowercase()
    )
}

struct RuntimeMeta {
    name: String,
    minimap_visible: bool,
    minimap_data_image: Option<String>,
    minimap_estate_image: Option<String>,
    skybox_fixed_time: Option<serde_json::Number>,
    /// Each texture's content hash, `None` where it names no content entry.
    skybox_textures: Vec<Option<String>>,
    fixed_adapter: Option<String>,
}

impl RuntimeMeta {
    /// Upstream's `extractWorldRuntimeMetadata` after `migrateConfiguration`:
    /// a truthy legacy `dclName` replaces `name`, `minimapVisible` replaces
    /// the whole `miniMapConfig` and `skybox` the whole `skyboxConfig`.
    fn from_scene(world_name: &str, scene: &WorldScene) -> Self {
        let wc = scene
            .entity
            .get("metadata")
            .and_then(|m| m.get("worldConfiguration"));
        let legacy = |key: &str| wc.and_then(|c| c.get(key)).filter(|v| js_truthy(v));
        let name = legacy("dclName")
            .or_else(|| wc.and_then(|c| c.get("name")))
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| world_name.to_string());
        let legacy_mini_map = legacy("minimapVisible").map(|v| serde_json::json!({ "visible": v }));
        let mini_map = legacy_mini_map
            .as_ref()
            .or_else(|| wc.and_then(|c| c.get("miniMapConfig")));
        let minimap_visible = mini_map
            .and_then(|m| m.get("visible"))
            .is_some_and(js_truthy);
        let legacy_skybox = legacy("skybox").map(|v| serde_json::json!({ "fixedTime": v }));
        let skybox = legacy_skybox
            .as_ref()
            .or_else(|| wc.and_then(|c| c.get("skyboxConfig")));
        let skybox_fixed_time = skybox
            .and_then(|s| s.get("fixedTime"))
            .and_then(Value::as_number)
            .cloned();

        let resolve = |filename: &str| -> Option<String> {
            content_file_hash(&scene.entity, filename).map(str::to_string)
        };
        let minimap_data_image = mini_map
            .and_then(|m| m.get("dataImage"))
            .and_then(Value::as_str)
            .and_then(resolve);
        let minimap_estate_image = mini_map
            .and_then(|m| m.get("estateImage"))
            .and_then(Value::as_str)
            .and_then(resolve);
        let skybox_textures = skybox
            .and_then(|s| s.get("textures"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|t| t.as_str().filter(|f| !f.is_empty()).and_then(resolve))
            .collect();
        let fixed_adapter = wc
            .and_then(|c| c.get("fixedAdapter"))
            .and_then(Value::as_str)
            .map(str::to_string);

        RuntimeMeta {
            name,
            minimap_visible,
            minimap_data_image,
            minimap_estate_image,
            skybox_fixed_time,
            skybox_textures,
            fixed_adapter,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ZERO_ADDRESS;
    use crate::queries::{WorldAbout, WorldRecord};
    use serde_json::json;

    fn scene(mirror_source: Option<&str>) -> WorldScene {
        WorldScene {
            entity_id: "bafy".into(),
            entity: json!({}),
            parcels: vec!["0,0".into()],
            deployer: if mirror_source.is_some() {
                ZERO_ADDRESS.into()
            } else {
                "0xabc".into()
            },
            mirror_source: mirror_source.map(str::to_string),
        }
    }

    #[test]
    fn locally_published_worlds_lose_the_ens_suffix() {
        assert_eq!(
            resolve_realm_name("swissverse.dcl.eth", None, true, true),
            "swissverse"
        );
    }

    #[test]
    fn mirrored_worlds_keep_the_ens_suffix() {
        assert_eq!(
            resolve_realm_name("swissverse.dcl.eth", None, true, false),
            "swissverse.dcl.eth"
        );
    }

    #[test]
    fn the_override_wins_over_every_policy() {
        assert_eq!(
            resolve_realm_name("swissverse.dcl.eth", Some(" swiss-cube "), false, false),
            "swiss-cube"
        );
        assert_eq!(
            resolve_realm_name("swissverse.dcl.eth", Some("keep.dcl.eth"), true, true),
            "keep.dcl.eth"
        );
    }

    #[test]
    fn a_blank_override_is_not_an_override() {
        assert_eq!(
            resolve_realm_name("plain-name", Some("   "), false, false),
            "plain-name"
        );
    }

    #[test]
    fn stripping_leaves_non_ens_and_bare_suffix_names_alone() {
        assert_eq!(
            resolve_realm_name("swiss-cube", None, true, true),
            "swiss-cube"
        );
        assert_eq!(resolve_realm_name(".dcl.eth", None, true, true), ".dcl.eth");
    }

    #[test]
    fn only_a_scene_without_a_mirror_source_counts_as_locally_published() {
        assert!(!is_locally_published(&[scene(Some("https://up.example"))]));
        assert!(is_locally_published(&[
            scene(Some("https://up.example")),
            scene(None)
        ]));
    }

    #[test]
    fn single_player_override_maps_to_offline_adapter() {
        assert_eq!(
            resolve_fixed_adapter(
                "foo",
                Some("offline:offline"),
                "https://worlds.example",
                true
            ),
            "fixed-adapter:offline:offline"
        );
    }

    #[test]
    fn no_override_falls_back_to_signed_login() {
        assert_eq!(
            resolve_fixed_adapter("Foo", None, "https://worlds.example", true),
            "fixed-adapter:signed-login:https://worlds.example/worlds/foo/comms"
        );
    }

    #[test]
    fn an_unreachable_sfu_serves_the_offline_adapter() {
        assert_eq!(
            resolve_fixed_adapter("foo", None, "https://worlds.example", false),
            "fixed-adapter:offline:offline"
        );
    }

    #[test]
    fn a_plain_http_node_still_advertises_signed_login() {
        assert_eq!(
            resolve_fixed_adapter("Foo", None, "http://127.0.0.1:5210", true),
            "fixed-adapter:signed-login:http://127.0.0.1:5210/worlds/foo/comms"
        );
    }

    #[test]
    fn a_public_url_under_the_worlds_prefix_reaches_the_nested_mint() {
        assert_eq!(
            resolve_fixed_adapter(
                "foo",
                None,
                "https://peer.example/worlds-content-server",
                true
            ),
            "fixed-adapter:signed-login:https://peer.example/worlds-content-server/worlds/foo/comms"
        );
    }

    fn entity(world_name: &str) -> Value {
        json!({
            "timestamp": 1000,
            "content": [
                {"file": "scene.json", "hash": "bafkreiscene"},
                {"file": "images/minimap.png", "hash": "bafkreiminimap"},
                {"file": "sky/1.png", "hash": "bafkreisky1"}
            ],
            "metadata": {
                "scene": {"base": "1,2", "parcels": ["1,2"]},
                "worldConfiguration": {
                    "name": world_name,
                    "miniMapConfig": {"visible": true, "dataImage": "images/minimap.png"},
                    "skyboxConfig": {"fixedTime": 36000, "textures": ["sky/1.png", "missing.png"]}
                }
            }
        })
    }

    #[test]
    fn about_points_scenes_and_files_at_this_node_and_keeps_mirrored_ens_names() {
        let cfg = super::super::Settings::for_tests("https://worlds.example.org/");
        let scenes = vec![WorldScene {
            entity_id: "bafkreientity".into(),
            entity: entity("mirrored.dcl.eth"),
            parcels: vec!["1,2".into()],
            deployer: ZERO_ADDRESS.into(),
            mirror_source: Some("https://upstream.example".into()),
        }];
        let about = build_about(
            &cfg,
            "mirrored.dcl.eth",
            WorldAbout {
                world: Some(WorldRecord {
                    name: "mirrored.dcl.eth".into(),
                    blocked_since: None,
                    quota_blocked_since: None,
                    spawn_coordinates: None,
                    skybox_time: Some(43200),
                    single_player: false,
                    realm_name_override: None,
                }),
                scenes,
            },
        )
        .unwrap();
        let mut v = serde_json::to_value(&about).unwrap();
        assert_eq!(
            v["configurations"]["scenesUrn"],
            json!([
                "urn:decentraland:entity:bafkreientity?=&baseUrl=https://worlds.example.org/worlds-content-server/contents/"
            ])
        );
        assert_eq!(v["configurations"]["realmName"], "mirrored.dcl.eth");
        assert_eq!(
            v["spawnCoordinates"],
            Value::Null,
            "the world row's spawn, null when it has none"
        );
        assert_eq!(
            v["configurations"]["skybox"]["fixedHour"],
            json!(43200),
            "the world row overrides the scene, as an integer"
        );
        // The unresolved `missing.png` links as `""`, in its place; the rest below.
        let textures = v["configurations"]["skybox"]["textures"]
            .as_array_mut()
            .unwrap();
        assert_eq!(textures.pop(), Some(json!("")));
        assert_eq!(
            v["configurations"]["skybox"]["textures"],
            json!(["https://worlds.example.org/worlds-content-server/contents/bafkreisky1"]),
            "a texture that names no content entry is dropped"
        );
        assert_eq!(v["configurations"]["minimap"]["enabled"], true);
        assert_eq!(
            v["configurations"]["minimap"]["dataImage"],
            "https://worlds.example.org/worlds-content-server/contents/bafkreiminimap"
        );
        assert_eq!(
            v["configurations"]["minimap"]["estateImage"], "http://127.0.0.1:5162/v1/estatemap.png",
            "a visible minimap without its own estate image falls back to the map service"
        );
        assert_eq!(
            v["comms"]["adapter"], "fixed-adapter:offline:offline",
            "a node without comms advertises the offline adapter"
        );
        assert_eq!(
            v["content"]["publicUrl"],
            "https://peer.example.org/content"
        );
        assert_eq!(v["healthy"], true);
        assert_eq!(v["acceptingUsers"], true);

        let online = super::super::Settings {
            comms_online: true,
            ..cfg.clone()
        };
        let local = build_about(
            &online,
            "Mine.dcl.eth",
            WorldAbout {
                world: None,
                scenes: vec![WorldScene {
                    entity_id: "bafkreientity".into(),
                    entity: entity("mine.dcl.eth"),
                    parcels: vec!["1,2".into()],
                    deployer: "0xabc".into(),
                    mirror_source: None,
                }],
            },
        )
        .unwrap();
        assert_eq!(local.configurations.realm_name, "mine");
        assert_eq!(
            local.comms.adapter,
            "fixed-adapter:signed-login:https://worlds.example.org/worlds/mine.dcl.eth/comms"
        );
    }

    #[test]
    fn a_scene_that_fixes_the_offline_adapter_stays_offline_with_comms_online() {
        let cfg = super::super::Settings {
            comms_online: true,
            ..super::super::Settings::for_tests("https://worlds.example.org")
        };
        let mut solo = entity("solo.dcl.eth");
        solo["metadata"]["worldConfiguration"]["fixedAdapter"] = json!("offline:offline");
        let about = build_about(
            &cfg,
            "solo.dcl.eth",
            WorldAbout {
                world: None,
                scenes: vec![WorldScene {
                    entity_id: "bafkreientity".into(),
                    entity: solo,
                    parcels: vec!["1,2".into()],
                    deployer: "0xabc".into(),
                    mirror_source: None,
                }],
            },
        )
        .unwrap();
        assert_eq!(about.comms.adapter, "fixed-adapter:offline:offline");
    }

    fn local_about(cfg: &super::super::Settings, world: Option<WorldRecord>) -> AboutResponse {
        build_about(
            cfg,
            "mine.dcl.eth",
            WorldAbout {
                world,
                scenes: vec![WorldScene {
                    entity_id: "bafkreientity".into(),
                    entity: entity("mine.dcl.eth"),
                    parcels: vec!["1,2".into()],
                    deployer: "0xabc".into(),
                    mirror_source: None,
                }],
            },
        )
        .unwrap()
    }

    #[test]
    fn a_single_player_world_stays_offline_with_comms_online() {
        let cfg = super::super::Settings {
            comms_online: true,
            ..super::super::Settings::for_tests("https://worlds.example.org")
        };
        let about = local_about(
            &cfg,
            Some(WorldRecord {
                name: "mine.dcl.eth".into(),
                blocked_since: None,
                quota_blocked_since: None,
                spawn_coordinates: None,
                skybox_time: None,
                single_player: true,
                realm_name_override: None,
            }),
        );
        assert_eq!(about.comms.adapter, "fixed-adapter:offline:offline");
    }

    #[test]
    fn configured_comms_follow_the_shared_sfu_liveness() {
        let alive = super::super::Settings {
            comms_online: true,
            ..super::super::Settings::for_tests("https://worlds.example.org")
        };
        let down = super::super::Settings {
            sfu: crate::content::about_comms::SfuHealth::down_for_tests(),
            ..alive.clone()
        };
        assert_eq!(
            local_about(&alive, None).comms.adapter,
            "fixed-adapter:signed-login:https://worlds.example.org/worlds/mine.dcl.eth/comms"
        );
        assert_eq!(
            local_about(&down, None).comms.adapter,
            "fixed-adapter:offline:offline",
            "an unreachable SFU falls back to single-player"
        );
    }

    #[test]
    fn a_world_without_scenes_is_404_and_a_blocked_world_401() {
        let cfg = super::super::Settings::for_tests("https://worlds.example.org");
        let err = build_about(
            &cfg,
            "none.dcl.eth",
            WorldAbout {
                world: None,
                scenes: vec![],
            },
        )
        .unwrap_err();
        assert!(err.status() == 404 && err.public_message().contains("no scenes deployed"));

        let blocked = WorldAbout {
            world: Some(WorldRecord {
                name: "b.dcl.eth".into(),
                blocked_since: Some(
                    chrono::DateTime::from_timestamp(1_700_000_000, 0)
                        .unwrap()
                        .naive_utc(),
                ),
                quota_blocked_since: None,
                spawn_coordinates: None,
                skybox_time: None,
                single_player: false,
                realm_name_override: None,
            }),
            scenes: vec![scene(Some("https://up.example"))],
        };
        let err = build_about(&cfg, "b.dcl.eth", blocked.clone()).unwrap_err();
        assert_eq!(err.status(), 401);
        assert!(
            err.public_message().ends_with(
                "blocked since Tue Nov 14 2023 22:13:20 GMT+0000 (Coordinated Universal Time) by an operator."
            ),
            "{err}"
        );

        // A quota block refuses only after its two-day grace.
        let quota = |age: chrono::TimeDelta| {
            let mut about = blocked.clone();
            let world = about.world.as_mut().unwrap();
            world.blocked_since = None;
            world.quota_blocked_since = Some(chrono::Utc::now().naive_utc() - age);
            about
        };
        assert!(build_about(&cfg, "b.dcl.eth", quota(chrono::TimeDelta::hours(47))).is_ok());
        let err = build_about(&cfg, "b.dcl.eth", quota(chrono::TimeDelta::hours(49))).unwrap_err();
        assert_eq!(err.status(), 401);
        assert!(
            err.public_message()
                .contains("as it exceeded its allowed storage space"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_deny_listed_world_is_404_before_any_read() {
        use tower::ServiceExt;
        let dir = std::env::temp_dir().join(format!(
            "catalyst-worlds-about-deny-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let state = Arc::new(crate::WorldsState::new(
            sqlx::PgPool::connect_lazy("postgres://postgres@127.0.0.1:1/postgres").unwrap(),
            Arc::new(catalyst_storage::Storage::from_config(&dir).await.unwrap()),
            super::super::Settings::for_tests("https://worlds.example.org"),
            None,
            crate::http::Outbound::for_tests(),
        ));
        state.names.seed(vec!["banned".to_string()]);
        let resp = crate::router(state)
            .oneshot(
                axum::http::Request::get("/world/Banned.dcl.eth/about")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        // http-commons' `NotFoundError` (`world-about-handler.spec.ts:30-37`).
        assert_eq!(
            body,
            json!({"error": "Not Found", "message": "World \"Banned.dcl.eth\" has no scene deployed."})
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn health_follows_the_local_content_sync_state() {
        let sync = Arc::new(crate::content::SyncStatus::default());
        let cfg = super::super::Settings {
            content_readiness: ContentReadiness::local(sync.clone()),
            ..super::super::Settings::for_tests("https://worlds.example.org")
        };
        let bootstrapping = local_about(&cfg, None);
        assert!(bootstrapping.healthy && bootstrapping.accepting_users);
        assert!(bootstrapping.content.healthy);
        sync.set_state(crate::content::SyncState::Syncing);
        let syncing = local_about(&cfg, None);
        assert!(syncing.healthy && syncing.accepting_users && syncing.content.healthy);
        sync.set_state(crate::content::SyncState::Failed);
        let failed = local_about(&cfg, None);
        assert!(!failed.healthy && !failed.accepting_users);
        assert!(!failed.content.healthy && failed.lambdas.healthy);
    }

    fn scene_with(entity_id: &str, entity: Value) -> WorldScene {
        WorldScene {
            entity_id: entity_id.into(),
            entity,
            parcels: vec!["1,2".into()],
            deployer: "0xabc".into(),
            mirror_source: None,
        }
    }

    /// `getMetadataForWorld` reads the newest scene only
    /// (`world-about-handler.spec.ts:500-555` pins one URN for two scenes).
    #[test]
    fn scenes_urn_names_the_newest_scene_only() {
        let cfg = super::super::Settings::for_tests("https://worlds.example.org");
        let about = build_about(
            &cfg,
            "two.dcl.eth",
            WorldAbout {
                world: None,
                scenes: vec![
                    scene_with("bafkreinewest", entity("two.dcl.eth")),
                    scene_with("bafkreiolder", entity("two.dcl.eth")),
                ],
            },
        )
        .unwrap();
        assert_eq!(about.configurations.scenes_urn.len(), 1);
        assert!(about.configurations.scenes_urn[0].contains("bafkreinewest"));
    }

    #[test]
    fn runtime_metadata_follows_upstream_migrate_configuration() {
        let cfg = super::super::Settings::for_tests("https://worlds.example.org");
        let about_of = |wc: Value| {
            let mut e = entity("x");
            e["metadata"]["worldConfiguration"] = wc;
            serde_json::to_value(
                build_about(
                    &cfg,
                    "legacy-world",
                    WorldAbout {
                        world: None,
                        scenes: vec![scene_with("bafkreientity", e)],
                    },
                )
                .unwrap(),
            )
            .unwrap()
        };

        // Legacy SDK6 keys: dclName, minimapVisible (drops miniMapConfig), skybox.
        let v = about_of(json!({
            "dclName": "Old Name",
            "name": "ignored",
            "minimapVisible": true,
            "miniMapConfig": {"visible": false, "dataImage": "images/minimap.png"},
            "skybox": 7200,
            "skyboxConfig": {"fixedTime": 1, "textures": ["sky/1.png"]}
        }));
        let c = &v["configurations"];
        assert_eq!(c["realmName"], "Old Name");
        assert_eq!(c["minimap"]["enabled"], true);
        assert_eq!(
            c["minimap"]["dataImage"], "http://127.0.0.1:5162/v1/minimap.png",
            "the legacy flag replaces the whole miniMapConfig"
        );
        assert_eq!(c["skybox"], json!({"fixedHour": 7200, "textures": []}));

        // No fixed time: the key is absent; unresolved textures link as "".
        let v = about_of(json!({
            "name": "",
            "skyboxConfig": {"textures": ["sky/1.png", "missing.png", 3]}
        }));
        let c = &v["configurations"];
        assert_eq!(c["realmName"], "legacy-world", "an empty name is missing");
        assert_eq!(c["skybox"].as_object().unwrap().len(), 1, "{c}");
        let textures = c["skybox"]["textures"].as_array().unwrap();
        assert_eq!(textures.len(), 3);
        assert!(
            textures[0]
                .as_str()
                .unwrap()
                .ends_with("/contents/bafkreisky1")
        );
        assert_eq!(textures[1..], [json!(""), json!("")]);
        assert_eq!(c["minimap"], json!({"enabled": false}));

        // A falsy legacy key is skipped: the current configs answer.
        let v = about_of(json!({
            "dclName": "",
            "name": "Current Name",
            "minimapVisible": false,
            "miniMapConfig": {"visible": true, "dataImage": "images/minimap.png"},
            "skybox": 0,
            "skyboxConfig": {"fixedTime": 3600, "textures": ["sky/1.png"]}
        }));
        let c = &v["configurations"];
        assert_eq!(c["realmName"], "Current Name");
        assert_eq!(c["minimap"]["enabled"], true);
        assert!(
            c["minimap"]["dataImage"]
                .as_str()
                .unwrap()
                .ends_with("/contents/bafkreiminimap"),
            "{c}"
        );
        assert_eq!(c["skybox"]["fixedHour"], 3600);
        assert_eq!(c["skybox"]["textures"].as_array().unwrap().len(), 1);
    }
}
