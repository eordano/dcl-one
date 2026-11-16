use std::fmt::Display;

use crate::{
    settings::{
        imposter_settings::ImposterSetting,
        player_settings::{JogSpeedSetting, RunJumpSetting},
        sensitivity::*,
    },
    SystemApi,
};
use ambient_brightness_setting::AmbientSetting;
use anyhow::anyhow;
use avatar_ghosts_setting::AvatarGhostsSetting;
use avatar_outline_setting::AvatarOutlineSetting;
use bevy::{
    app::{Plugin, Update},
    core_pipeline::prepass::{DepthPrepass, NormalPrepass},
    ecs::{
        schedule::ScheduleLabel,
        system::{StaticSystemParam, SystemParam, SystemParamItem},
    },
    pbr::ScreenSpaceAmbientOcclusion,
    platform::collections::HashSet,
    prelude::*,
};
#[cfg(not(target_arch = "wasm32"))]
use cache_size::CacheSizeSetting;
use cel_shading_setting::CelShadingSetting;
use common::structs::DmPrivacy;
#[cfg(not(target_arch = "wasm32"))]
use common::structs::SsaoSetting;
use common::{
    sets::SceneSets,
    structs::{
        AaSetting, AppConfig, BloomSetting, CameraSmoothing, DofSetting, FogSetting,
        GraphicsSettings, ParcelGrassConfig, ParcelGrassSetting, PlayerLanguage,
        PointAtMarkerVisibility, PreviewMode, PrimaryCamera, SessionGraphics, ShadowSetting,
        SkyReflectionSetting, WindowSetting,
    },
};
use constrain_ui::ConstrainUiSetting;
use frame_rate::FpsTargetSetting;
use language_setting::LanguageSetting;
use load_distance::{LoadDistanceSetting, UnloadDistanceSetting};
use max_avatars::MaxAvatarsSetting;
use max_downloads::MaxDownloadsSetting;
use oob_setting::OobSetting;
use player_settings::{JumpSetting, RunSpeedSetting, WalkSpeedSetting};
use scene_threads::SceneThreadsSetting;
use shadow_settings::{LightCountSetting, ShadowCasterCountSetting, ShadowDistanceSetting};
use video_threads::VideoThreadsSetting;
use volume_settings::{
    AvatarVolumeSetting, MasterVolumeSetting, SceneVolumeSetting, SystemVolumeSetting,
    VoiceChatVolumeSetting, VoiceVolumeSetting,
};

pub mod aa_settings;
pub mod ambient_brightness_setting;
pub mod avatar_ghosts_setting;
pub mod avatar_outline_setting;
pub mod bloom_settings;
pub mod cache_size;
pub mod camera_smoothing;
pub mod cel_shading_setting;
pub mod constrain_ui;
mod debug_quality;
pub mod dm_privacy;
pub mod dof_setting;
pub mod fog_settings;
pub mod frame_rate;
pub mod imposter_settings;
pub mod language_setting;
pub mod load_distance;
pub mod max_avatars;
pub mod max_downloads;
pub mod oob_setting;
pub mod parcel_grass_settings;
pub mod player_settings;
pub mod point_at_marker_visibility;
pub mod render_scale_setting;
pub mod scene_threads;
pub mod sensitivity;
pub mod shadow_settings;
pub mod sky_reflection_setting;
pub mod ssao_setting;
pub mod video_threads;
pub mod volume_settings;
pub mod window_settings;

/// Inserts both prepasses while the avatar outline (edge detection) is on or the camera has SSAO
/// (native only), and removes them otherwise, keeping the primary camera's depth prepass, which
/// the jump marker reads. Nothing else needs them: DoF, stars and the fork's focus target use the
/// main-pass depth, and the hover glow follows whatever prepasses the view has.
///
/// SSAO is read from the camera when the command applies, so the outline and SSAO settings reach
/// the same result in either order.
pub fn sync_prepasses(cmds: &mut EntityCommands, graphics: &GraphicsSettings) {
    if graphics.avatar_outline {
        cmds.try_insert((DepthPrepass, NormalPrepass));
    } else {
        cmds.queue(|mut camera: EntityWorldMut| {
            if camera.contains::<ScreenSpaceAmbientOcclusion>() {
                camera.insert((DepthPrepass, NormalPrepass));
            } else if camera.contains::<PrimaryCamera>() {
                camera.remove::<NormalPrepass>();
            } else {
                camera.remove::<(DepthPrepass, NormalPrepass)>();
            }
        });
    }
}

pub struct SettingBridgePlugin;

#[derive(Event)]
pub struct NewCameraEvent(pub Entity);

impl Plugin for SettingBridgePlugin {
    fn build(&self, app: &mut App) {
        let is_preview = app.world().resource::<PreviewMode>().is_preview;

        fn apply_to_camera<S: AppSetting>(
            mut commands: Commands,
            config: Res<AppConfig>,
            mut new_camera_events: EventReader<NewCameraEvent>,
            param: StaticSystemParam<S::Param>,
        ) {
            let param = param.into_inner();
            for ev in new_camera_events.read() {
                let setting = S::load(&config);
                setting.apply_to_camera(&param, commands.reborrow(), ev.0);
            }
        }

        fn add_int_setting<T: IntAppSetting>(
            app: &mut App,
            settings: &mut Settings,
            schedule: &mut Schedule,
            config: &AppConfig,
        ) {
            settings.add_int_setting::<T>(config);
            schedule.add_systems(apply_setting::<T>);
            app.add_systems(
                Update,
                apply_to_camera::<T>.in_set(SceneSets::RestrictedActions),
            );
        }

        fn add_enum_setting<T: EnumAppSetting>(
            app: &mut App,
            settings: &mut Settings,
            schedule: &mut Schedule,
            config: &AppConfig,
        ) {
            settings.add_enum_setting::<T>(config);
            schedule.add_systems(apply_setting::<T>);
            app.add_systems(
                Update,
                apply_to_camera::<T>.in_set(SceneSets::RestrictedActions),
            );
        }

        let mut settings = Settings {
            settings: Vec::default(),
        };
        app.add_event::<NewCameraEvent>();
        app.add_systems(Update, handle_settings);

        let mut schedule = Schedule::new(ApplyAppSettingsLabel);
        let config = app.world().resource::<AppConfig>().clone();

        add_int_setting::<ShadowDistanceSetting>(app, &mut settings, &mut schedule, &config);
        add_int_setting::<LightCountSetting>(app, &mut settings, &mut schedule, &config);
        add_int_setting::<ShadowCasterCountSetting>(app, &mut settings, &mut schedule, &config);

        // special case for ordering
        settings.add_enum_setting::<ShadowSetting>(&config);
        schedule.add_systems(
            apply_setting::<ShadowSetting>.after(apply_setting::<ShadowDistanceSetting>),
        );
        app.add_systems(
            Update,
            apply_to_camera::<ShadowSetting>.in_set(SceneSets::RestrictedActions),
        );

        if !is_preview {
            add_enum_setting::<ImposterSetting>(app, &mut settings, &mut schedule, &config);
        }
        add_enum_setting::<ParcelGrassSetting>(app, &mut settings, &mut schedule, &config);
        add_enum_setting::<FogSetting>(app, &mut settings, &mut schedule, &config);
        add_enum_setting::<BloomSetting>(app, &mut settings, &mut schedule, &config);
        add_enum_setting::<DofSetting>(app, &mut settings, &mut schedule, &config);
        #[cfg(not(target_arch = "wasm32"))]
        add_enum_setting::<SsaoSetting>(app, &mut settings, &mut schedule, &config);
        add_enum_setting::<SkyReflectionSetting>(app, &mut settings, &mut schedule, &config);
        add_enum_setting::<OobSetting>(app, &mut settings, &mut schedule, &config);
        add_enum_setting::<CelShadingSetting>(app, &mut settings, &mut schedule, &config);
        add_enum_setting::<AvatarOutlineSetting>(app, &mut settings, &mut schedule, &config);
        add_enum_setting::<AvatarGhostsSetting>(app, &mut settings, &mut schedule, &config);
        add_enum_setting::<AaSetting>(app, &mut settings, &mut schedule, &config);
        add_int_setting::<AmbientSetting>(app, &mut settings, &mut schedule, &config);
        if is_fullscreen_available() {
            add_enum_setting::<WindowSetting>(app, &mut settings, &mut schedule, &config);
        }

        if !is_preview {
            add_int_setting::<LoadDistanceSetting>(app, &mut settings, &mut schedule, &config);
            add_int_setting::<UnloadDistanceSetting>(app, &mut settings, &mut schedule, &config);
        }

        add_enum_setting::<FpsTargetSetting>(app, &mut settings, &mut schedule, &config);
        add_int_setting::<render_scale_setting::RenderScaleSetting>(
            app,
            &mut settings,
            &mut schedule,
            &config,
        );
        add_int_setting::<SceneThreadsSetting>(app, &mut settings, &mut schedule, &config);
        add_int_setting::<MaxAvatarsSetting>(app, &mut settings, &mut schedule, &config);
        add_int_setting::<MasterVolumeSetting>(app, &mut settings, &mut schedule, &config);
        add_int_setting::<SceneVolumeSetting>(app, &mut settings, &mut schedule, &config);
        add_int_setting::<VoiceVolumeSetting>(app, &mut settings, &mut schedule, &config);
        add_int_setting::<VoiceChatVolumeSetting>(app, &mut settings, &mut schedule, &config);
        add_int_setting::<SystemVolumeSetting>(app, &mut settings, &mut schedule, &config);
        add_int_setting::<AvatarVolumeSetting>(app, &mut settings, &mut schedule, &config);

        add_enum_setting::<DmPrivacy>(app, &mut settings, &mut schedule, &config);
        app.init_resource::<PlayerLanguage>();
        add_enum_setting::<LanguageSetting>(app, &mut settings, &mut schedule, &config);

        add_enum_setting::<ConstrainUiSetting>(app, &mut settings, &mut schedule, &config);
        add_int_setting::<WalkSpeedSetting>(app, &mut settings, &mut schedule, &config);
        add_int_setting::<JogSpeedSetting>(app, &mut settings, &mut schedule, &config);
        add_int_setting::<RunSpeedSetting>(app, &mut settings, &mut schedule, &config);
        add_int_setting::<JumpSetting>(app, &mut settings, &mut schedule, &config);
        add_int_setting::<RunJumpSetting>(app, &mut settings, &mut schedule, &config);

        add_int_setting::<PointerSensitivitySetting>(app, &mut settings, &mut schedule, &config);
        add_int_setting::<CameraZoomSensitivitySetting>(app, &mut settings, &mut schedule, &config);
        add_int_setting::<ScrollSensitivitySetting>(app, &mut settings, &mut schedule, &config);
        add_int_setting::<MovementSensitivitySetting>(app, &mut settings, &mut schedule, &config);
        add_int_setting::<CameraSensitivitySetting>(app, &mut settings, &mut schedule, &config);
        add_enum_setting::<CameraSmoothing>(app, &mut settings, &mut schedule, &config);

        add_int_setting::<VideoThreadsSetting>(app, &mut settings, &mut schedule, &config);
        add_int_setting::<MaxDownloadsSetting>(app, &mut settings, &mut schedule, &config);
        // Only the native IPFS disk cache is trimmed against this setting.
        #[cfg(not(target_arch = "wasm32"))]
        add_enum_setting::<CacheSizeSetting>(app, &mut settings, &mut schedule, &config);
        add_enum_setting::<PointAtMarkerVisibility>(app, &mut settings, &mut schedule, &config);

        app.insert_resource(settings);
        app.add_plugins(debug_quality::DebugQualityPlugin);
        app.insert_resource(ApplyAppSettingsSchedule(schedule));
        app.init_resource::<ActiveCameras>();
        app.init_resource::<SessionGraphics>();
        app.add_systems(
            Update,
            (
                record_cameras,
                apply_settings.run_if(resource_changed::<AppConfig>),
                refresh_settings.run_if(resource_changed::<AppConfig>),
                apply_session_grass,
            )
                .chain(),
        );
    }
}

pub enum SettingCategory {
    Gameplay,
    Graphics,
    Audio,
    Chat,
    Performance,
    Controls,
}

impl Display for SettingCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SettingCategory::Gameplay => "Gameplay",
            SettingCategory::Graphics => "Graphics",
            SettingCategory::Audio => "Audio",
            SettingCategory::Chat => "Chat",
            SettingCategory::Performance => "Performance",
            SettingCategory::Controls => "Controls",
        })
    }
}

pub trait AppSetting: Eq + Send + Sync + 'static {
    type Param: SystemParam + 'static;
    fn title() -> String;
    fn description(&self) -> String;
    fn category() -> SettingCategory;
    fn load(config: &AppConfig) -> Self;
    fn save(&self, config: &mut AppConfig);
    fn apply(
        &self,
        param: SystemParamItem<Self::Param>,
        mut commands: Commands,
        cameras: &HashSet<Entity>,
    ) {
        for &cam in cameras {
            self.apply_to_camera(&param, commands.reborrow(), cam)
        }
    }
    fn apply_to_camera(
        &self,
        _param: &SystemParamItem<Self::Param>,
        _commands: Commands,
        _camera_entity: Entity,
    ) {
    }
}

pub trait EnumAppSetting: AppSetting + Sized + std::fmt::Debug {
    fn variants() -> Vec<Self>;
    fn name(&self) -> String;
    /// True while the value is not yet known (reported as -1, which the panel shows disabled).
    fn is_unset(_config: &AppConfig) -> bool {
        false
    }
}

pub trait IntAppSetting: AppSetting + Sized + std::fmt::Debug {
    fn from_int(value: i32) -> Self;
    fn value(&self) -> i32;
    fn min() -> i32;
    fn max() -> i32;
    fn scale() -> f32 {
        1.0
    }
    fn display(&self) -> String {
        format!("{}", self.value())
    }
}

pub use system_api_types::{NamedVariant, SettingInfo};

pub struct Setting {
    pub info: SettingInfo,
    apply: Option<
        Box<dyn Fn(&mut AppConfig, f32) -> Result<f32, anyhow::Error> + Send + Sync + 'static>,
    >,
    /// The displayed value for the current config, for settings changed outside `set_value`.
    load: Box<dyn Fn(&AppConfig) -> f32 + Send + Sync + 'static>,
}

#[derive(Resource)]
pub struct Settings {
    pub settings: Vec<Setting>,
}

impl Settings {
    pub fn get(&self) -> Vec<SettingInfo> {
        self.settings.iter().map(|s| s.info.clone()).collect()
    }

    pub fn set_value(
        &mut self,
        config: &mut AppConfig,
        name: &str,
        value: f32,
    ) -> Result<(), anyhow::Error> {
        let apply = self
            .settings
            .iter_mut()
            .find(|s| s.info.name == name)
            .ok_or(anyhow!(format!("{name} not found")))?
            .apply
            .take()
            .unwrap();
        let res = (apply)(config, value)?;
        let setting = self
            .settings
            .iter_mut()
            .find(|s| s.info.name == name)
            .unwrap();

        setting.apply = Some(apply);
        setting.info.value = res;
        Ok(())
    }

    /// Re-reads every displayed value from `config`.
    pub fn refresh(&mut self, config: &AppConfig) {
        for setting in self.settings.iter_mut() {
            setting.info.value = (setting.load)(config);
        }
    }

    pub fn add_int_setting<S: IntAppSetting>(&mut self, config: &AppConfig) {
        let value = S::load(config);
        self.settings.push(Setting {
            info: SettingInfo {
                name: S::title(),
                category: S::category().to_string(),
                description: S::description(&value),
                min_value: (S::min() as f32 * S::scale()).min(S::max() as f32 * S::scale()),
                max_value: (S::min() as f32 * S::scale()).max(S::max() as f32 * S::scale()),
                named_variants: Default::default(),
                value: value.value() as f32 * S::scale(),
                default: S::load(&AppConfig::default()).value() as f32 * S::scale(),
                step_size: S::scale().abs(),
            },
            apply: Some(Box::new(
                |config: &mut AppConfig, value: f32| -> Result<f32, anyhow::Error> {
                    let new_value = S::from_int((value / S::scale()).round() as i32);
                    new_value.save(config);
                    Ok(new_value.value() as f32 * S::scale())
                },
            )),
            load: Box::new(|config: &AppConfig| S::load(config).value() as f32 * S::scale()),
        });
    }

    pub fn add_enum_setting<S: EnumAppSetting>(&mut self, config: &AppConfig) {
        fn index_of<S: EnumAppSetting>(value: &S) -> usize {
            S::variants().iter().position(|s| s == value).unwrap_or(0)
        }
        fn displayed<S: EnumAppSetting>(config: &AppConfig) -> f32 {
            if S::is_unset(config) {
                -1.0
            } else {
                index_of(&S::load(config)) as f32
            }
        }
        let value = S::load(config);
        let default_index = index_of(&S::load(&AppConfig::default()));
        self.settings.push(Setting {
            info: SettingInfo {
                name: S::title(),
                category: S::category().to_string(),
                description: S::description(&value),
                min_value: 0.0,
                max_value: S::variants().len() as f32,
                named_variants: S::variants()
                    .into_iter()
                    .map(|v| NamedVariant {
                        name: v.name(),
                        description: v.description(),
                    })
                    .collect(),
                value: displayed::<S>(config),
                default: default_index as f32,
                step_size: 1.0,
            },
            apply: Some(Box::new(
                |config: &mut AppConfig, value: f32| -> Result<f32, anyhow::Error> {
                    S::variants()
                        .get(value as usize)
                        .ok_or(anyhow::anyhow!("invalid variant index"))?
                        .save(config);
                    Ok(value as usize as f32)
                },
            )),
            load: Box::new(|config: &AppConfig| displayed::<S>(config)),
        });
    }
}

// Gets and sets must be handled in arrival order: the scene->engine channel is FIFO, so a
// GetSettings queued after SetSettings is only guaranteed to reflect them if a single system
// processes both event kinds in sequence.
fn handle_settings(
    mut ev: EventReader<SystemApi>,
    mut config: ResMut<AppConfig>,
    mut settings: ResMut<Settings>,
) {
    for ev in ev.read() {
        match ev {
            SystemApi::GetSettings(sender) => {
                sender.send(settings.settings.iter().map(|s| s.info.clone()).collect());
            }
            SystemApi::SetSetting(name, val) => {
                if let Err(e) = settings.set_value(&mut config, name, *val) {
                    error!("Error setting {name}: {e}");
                }
            }
            _ => (),
        }
    }
}

// after the settings schedule, which writes the player's own grass when it changes; like
// `apply_setting`, only a changed value is written, since every write rebuilds the grass
fn apply_session_grass(
    session: Res<SessionGraphics>,
    config: Res<AppConfig>,
    mut grass: ResMut<ParcelGrassConfig>,
    mut applied: Local<Option<ParcelGrassSetting>>,
) {
    let setting = session.parcel_grass(&config);
    if *applied != Some(setting) {
        *applied = Some(setting);
        *grass = *setting;
    }
}

fn refresh_settings(mut settings: ResMut<Settings>, config: Res<AppConfig>) {
    settings.refresh(&config);
}

#[derive(Resource)]
pub struct ApplyAppSettingsSchedule(Schedule);

#[derive(ScheduleLabel, Hash, PartialEq, Eq, Clone, Copy, Debug)]
pub struct ApplyAppSettingsLabel;

fn apply_settings(world: &mut World) {
    world.resource_scope(
        |world: &mut World, mut schedule: Mut<ApplyAppSettingsSchedule>| {
            schedule.0.run(world);
        },
    );

    platform::write_config_file(world.resource::<AppConfig>());
}

#[derive(Resource, Default)]
pub struct ActiveCameras(HashSet<Entity>);

impl ActiveCameras {
    pub fn get<'a>(&'a mut self, commands: &mut Commands) -> &'a HashSet<Entity> {
        self.0.retain(|c| commands.get_entity(*c).is_ok());
        &self.0
    }
}

// Only a changed value is applied: the config also changes for reasons that are no setting
// (terms, home scene), and re-applying touches every camera and light.
fn apply_setting<S: AppSetting>(
    params: StaticSystemParam<S::Param>,
    config: Res<AppConfig>,
    mut commands: Commands,
    mut cameras: ResMut<ActiveCameras>,
    mut applied: Local<Option<S>>,
) {
    let setting = S::load(&config);
    if applied.as_ref() == Some(&setting) {
        return;
    }
    let cameras = cameras.get(&mut commands);
    setting.apply(params.into_inner(), commands, cameras);
    *applied = Some(setting);
}

pub fn record_cameras(
    mut cameras: ResMut<ActiveCameras>,
    mut new_cams: EventReader<NewCameraEvent>,
) {
    for ev in new_cams.read() {
        cameras.0.insert(ev.0);
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[inline(always)]
fn is_fullscreen_available() -> bool {
    true
}

// On the web the engine runs on a worker with no document; fullscreen is driven by the page.
#[cfg(target_arch = "wasm32")]
#[inline(always)]
fn is_fullscreen_available() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepasses_follow_the_avatar_outline() {
        let mut world = World::new();
        let primary = world
            .spawn((PrimaryCamera::default(), DepthPrepass, NormalPrepass))
            .id();
        let texture = world.spawn((DepthPrepass, NormalPrepass)).id();
        let off = GraphicsSettings {
            avatar_outline: false,
            ..Default::default()
        };
        let on = GraphicsSettings {
            avatar_outline: true,
            ..Default::default()
        };
        let mut commands = world.commands();
        sync_prepasses(&mut commands.entity(primary), &off);
        sync_prepasses(&mut commands.entity(texture), &off);
        world.flush();
        assert!(world.entity(primary).contains::<DepthPrepass>());
        assert!(!world.entity(primary).contains::<NormalPrepass>());
        assert!(!world.entity(texture).contains::<DepthPrepass>());
        assert!(!world.entity(texture).contains::<NormalPrepass>());

        let mut commands = world.commands();
        sync_prepasses(&mut commands.entity(primary), &on);
        sync_prepasses(&mut commands.entity(texture), &on);
        world.flush();
        for camera in [primary, texture] {
            assert!(world.entity(camera).contains::<DepthPrepass>());
            assert!(world.entity(camera).contains::<NormalPrepass>());
        }
    }

    #[test]
    fn ssao_keeps_both_prepasses_with_the_outline_off() {
        let mut world = World::new();
        let primary = world.spawn((PrimaryCamera::default(), DepthPrepass)).id();
        let texture = world.spawn_empty().id();
        let off = GraphicsSettings {
            avatar_outline: false,
            ..Default::default()
        };

        // the outline setting applies after SSAO reached the cameras
        for camera in [primary, texture] {
            world
                .entity_mut(camera)
                .insert(ScreenSpaceAmbientOcclusion::default());
        }
        let mut commands = world.commands();
        sync_prepasses(&mut commands.entity(primary), &off);
        sync_prepasses(&mut commands.entity(texture), &off);
        world.flush();
        for camera in [primary, texture] {
            assert!(world.entity(camera).contains::<DepthPrepass>());
            assert!(world.entity(camera).contains::<NormalPrepass>());
        }

        // the outline setting applies first and strips them, then SSAO arrives
        for camera in [primary, texture] {
            world
                .entity_mut(camera)
                .remove::<ScreenSpaceAmbientOcclusion>();
        }
        let mut commands = world.commands();
        sync_prepasses(&mut commands.entity(primary), &off);
        sync_prepasses(&mut commands.entity(texture), &off);
        world.flush();
        assert!(!world.entity(primary).contains::<NormalPrepass>());
        assert!(!world.entity(texture).contains::<DepthPrepass>());
        let mut commands = world.commands();
        for camera in [primary, texture] {
            let mut cmds = commands.entity(camera);
            cmds.try_insert(ScreenSpaceAmbientOcclusion::default());
            sync_prepasses(&mut cmds, &off);
        }
        world.flush();
        for camera in [primary, texture] {
            assert!(world.entity(camera).contains::<DepthPrepass>());
            assert!(world.entity(camera).contains::<NormalPrepass>());
        }

        // SSAO off again: back to the outline-off layout
        let mut commands = world.commands();
        for camera in [primary, texture] {
            let mut cmds = commands.entity(camera);
            cmds.remove::<ScreenSpaceAmbientOcclusion>();
            sync_prepasses(&mut cmds, &off);
        }
        world.flush();
        assert!(world.entity(primary).contains::<DepthPrepass>());
        assert!(!world.entity(primary).contains::<NormalPrepass>());
        assert!(!world.entity(texture).contains::<DepthPrepass>());
        assert!(!world.entity(texture).contains::<NormalPrepass>());
    }
}
