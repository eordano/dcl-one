//! Scene-root environment overrides. The active parcel owns them; leaving it restores the clock.

use bevy::{
    image::{ImageAddressMode, ImageLoaderSettings, ImageSampler, ImageSamplerDescriptor},
    pbr::{ExtendedMaterial, MaterialExtension},
    prelude::*,
    render::{
        render_asset::RenderAssetUsages,
        render_resource::{AsBindGroup, ShaderRef, ShaderType},
    },
};
use bevy_atmosphere::skybox::SkyBoxMaterial;
use bevy_console::ConsoleCommand;
use common::{
    sets::SceneSets,
    structs::{PrimaryUser, TimeOfDay},
};
use console::{AddDebugSection, DebugSection, DoAddConsoleCommand};
use dcl_component::{
    proto_components::{
        common::ColorGradient,
        sdk::components::{PbEnvironmentFog, PbSkybox, PbSun},
    },
    SceneComponentId,
};
use dcl_types::interface::ComponentPosition;
use ipfs::IpfsAssetServer;
use scene_runner::{
    renderer_context::RendererSceneContext, update_world::AddCrdtInterfaceExt, ContainingScene,
};

pub struct SceneEnvironmentPlugin;

impl Plugin for SceneEnvironmentPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SceneEnvironment>()
            .add_plugins(MaterialPlugin::<SceneSkyMaterial> {
                prepass_enabled: false,
                shadows_enabled: false,
                ..default()
            });
        app.add_crdt_lww_component::<PbSkybox, Skybox>(
            SceneComponentId::SKYBOX,
            ComponentPosition::RootOnly,
        );
        app.add_crdt_lww_component::<PbSun, Sun>(
            SceneComponentId::SUN,
            ComponentPosition::RootOnly,
        );
        app.add_crdt_lww_component::<PbEnvironmentFog, EnvironmentFog>(
            SceneComponentId::ENVIRONMENT_FOG,
            ComponentPosition::RootOnly,
        );
        app.add_systems(
            Update,
            update_environment
                .in_set(SceneSets::PostLoop)
                .before(crate::apply_global_light),
        )
        .add_systems(Update, update_sky_material.after(crate::apply_global_light))
        .add_console_command::<EnvironmentCommand, _>(environment_command)
        .add_debug_section(DebugSection {
            title: "Scene environment",
            query: "/scene_environment",
            fields: &[
                ("Scene", "scene"),
                ("Texture state", "textureState"),
                ("Sun visible", "sunVisible"),
                ("Sun intensity multiplier", "sunIntensity"),
                ("Sun direction", "sunDirection"),
                ("Sun color", "sunColor"),
                ("Fog density per metre", "fogDensity"),
                ("Fog color", "fogColor"),
                ("Sky zenith", "zenith"),
                ("Sky horizon", "horizon"),
                ("Sky nadir", "nadir"),
                ("Sky rim", "rim"),
                ("Cloud opacity", "cloudOpacity"),
                ("Cloud radians per second", "cloudSpeed"),
                ("Cloud color", "cloudColor"),
                ("Star brightness", "starsBrightness"),
            ],
            actions: &[
                ("Midnight", "/time 0 0"),
                ("Noon", "/time 12 0"),
                ("Resume scene time", "/time --resume"),
            ],
            inputs: &[],
        });
    }
}

macro_rules! component {
    ($name:ident, $proto:ty) => {
        #[derive(Component)]
        struct $name($proto);
        impl From<$proto> for $name {
            fn from(value: $proto) -> Self {
                Self(value)
            }
        }
    };
}
component!(Skybox, PbSkybox);
component!(Sun, PbSun);
component!(EnvironmentFog, PbEnvironmentFog);

#[derive(Resource, Default, Clone, PartialEq)]
pub(super) struct SceneEnvironment {
    pub source: Option<Entity>,
    pub zenith: Option<Vec3>,
    pub horizon: Option<Vec3>,
    pub nadir: Option<Vec3>,
    pub rim: Option<Vec3>,
    pub sun_color: Option<Vec3>,
    pub sun_direction: Option<Vec3>,
    pub sun_intensity: Option<f32>,
    pub sun_visible: Option<bool>,
    pub fog_color: Option<Vec3>,
    pub fog_density: Option<f32>,
    pub cloud_color: Option<Vec3>,
    pub cloud_opacity: Option<f32>,
    pub cloud_speed: Option<f32>,
    pub star_brightness: Option<f32>,
    pub texture: Option<Handle<Image>>,
    pub texture_state: &'static str,
}

#[derive(Default)]
struct TextureSlot {
    source: Option<(Entity, std::sync::Arc<str>, String)>,
    handle: Option<Handle<Image>>,
}

impl TextureSlot {
    fn select(
        &mut self,
        source: Option<(Entity, std::sync::Arc<str>, String)>,
        load: impl FnOnce(&str) -> Option<Handle<Image>>,
    ) {
        if self.source != source {
            // Drop the old request before exposing the new scene; late completions cannot win.
            self.handle = source.as_ref().and_then(|(_, _, name)| load(name));
            self.source = source;
        }
    }
}

fn valid_sky_image(image: &Image) -> bool {
    image.height() > 0 && image.width() == image.height().saturating_mul(2)
}

fn finite(value: Option<f32>, min: f32, max: f32) -> Option<f32> {
    value.filter(|v| v.is_finite()).map(|v| v.clamp(min, max))
}

/// Scene input is bounded before sorting. Rendering uses eight uniformly sampled keys.
fn sample_gradient(gradient: Option<&ColorGradient>, time: f32) -> Option<Vec3> {
    let mut keys: Vec<(f32, Vec3)> = gradient?
        .keys
        .iter()
        .take(64)
        .filter_map(|key| {
            let c = key.color.as_ref()?;
            let color = Vec3::new(c.r, c.g, c.b);
            (key.time.is_finite() && color.is_finite()).then_some((key.time.clamp(0.0, 1.0), color))
        })
        .collect();
    keys.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut unique: Vec<(f32, Vec3)> = Vec::with_capacity(keys.len());
    for key in keys {
        if unique.last().is_some_and(|previous| {
            (previous.0 - key.0).abs()
                < (1e-6 * previous.0.abs().max(key.0.abs())).max(f32::from_bits(8))
        }) {
            unique.pop();
        }
        unique.push(key);
    }
    if unique.len() > 8 {
        unique = (0..8)
            .map(|n| {
                let t = n as f32 / 7.0;
                (t, sample_keys(&unique, t).unwrap())
            })
            .collect();
    }
    sample_keys(&unique, time.clamp(0.0, 1.0))
}

fn sample_keys(keys: &[(f32, Vec3)], time: f32) -> Option<Vec3> {
    let first = keys.first()?;
    if time <= first.0 {
        return Some(first.1);
    }
    for pair in keys.windows(2) {
        if time <= pair[1].0 {
            return Some(
                pair[0]
                    .1
                    .lerp(pair[1].1, (time - pair[0].0) / (pair[1].0 - pair[0].0)),
            );
        }
    }
    Some(keys.last().unwrap().1)
}

fn sample_components(
    source: Entity,
    sky: Option<&PbSkybox>,
    sun: Option<&PbSun>,
    fog: Option<&PbEnvironmentFog>,
    transform: &GlobalTransform,
    time: f32,
) -> SceneEnvironment {
    let colors = sky.and_then(|s| s.sky_colors.as_ref());
    let clouds = sky.and_then(|s| s.clouds.as_ref());
    let horizon = sample_gradient(colors.and_then(|c| c.horizon.as_ref()), time);
    let sun_direction = sun.and_then(|s| s.direction.as_ref()).and_then(|d| {
        let direction = transform
            .affine()
            .transform_vector3(Vec3::new(d.x, d.y, -d.z));
        direction.try_normalize()
    });
    SceneEnvironment {
        source: Some(source),
        zenith: sample_gradient(colors.and_then(|c| c.zenith.as_ref()), time),
        horizon,
        nadir: sample_gradient(colors.and_then(|c| c.nadir.as_ref()), time),
        rim: sample_gradient(colors.and_then(|c| c.rim.as_ref()), time).or(horizon),
        sun_color: sample_gradient(sun.and_then(|s| s.color.as_ref()), time),
        sun_direction,
        sun_intensity: finite(sun.and_then(|s| s.intensity), 0.0, f32::MAX),
        sun_visible: sun.and_then(|s| s.visible),
        fog_color: sample_gradient(fog.and_then(|f| f.color.as_ref()), time),
        fog_density: finite(fog.and_then(|f| f.density), 0.0, f32::MAX),
        cloud_color: sample_gradient(clouds.and_then(|c| c.color.as_ref()), time),
        cloud_opacity: finite(clouds.and_then(|c| c.opacity), 0.0, 1.0),
        cloud_speed: finite(clouds.and_then(|c| c.speed), -f32::MAX, f32::MAX),
        star_brightness: finite(
            sky.and_then(|s| s.stars.as_ref())
                .and_then(|s| s.brightness),
            0.0,
            f32::MAX,
        ),
        texture_state: "unset",
        ..default()
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn update_environment(
    player: Query<Entity, With<PrimaryUser>>,
    containing: ContainingScene,
    scenes: Query<(
        &RendererSceneContext,
        &GlobalTransform,
        Option<&Skybox>,
        Option<&Sun>,
        Option<&EnvironmentFog>,
    )>,
    time: Res<TimeOfDay>,
    ipfs: IpfsAssetServer,
    images: Res<Assets<Image>>,
    server: Res<AssetServer>,
    mut state: ResMut<SceneEnvironment>,
    mut slot: Local<TextureSlot>,
) {
    let selected = player
        .single()
        .ok()
        .and_then(|p| containing.get_parcel_oow(p));
    let Some((entity, (context, transform, sky, sun, fog))) =
        selected.and_then(|e| scenes.get(e).ok().map(|s| (e, s)))
    else {
        state.set_if_neq(SceneEnvironment::default());
        *slot = TextureSlot::default();
        return;
    };
    if sky.is_none() && sun.is_none() && fog.is_none() {
        state.set_if_neq(SceneEnvironment::default());
        *slot = TextureSlot::default();
        return;
    }
    let mut next = sample_components(
        entity,
        sky.map(|s| &s.0),
        sun.map(|s| &s.0),
        fog.map(|s| &s.0),
        transform,
        time.elapsed_secs().rem_euclid(86400.0) / 86400.0,
    );
    let source = sky
        .and_then(|s| s.0.skybox_texture.as_ref())
        .filter(|s| !s.is_empty())
        .map(|s| (entity, context.hash.clone(), s.clone()));
    slot.select(source, |name| {
        ipfs.load_content_file_with_settings::<Image, ImageLoaderSettings>(
            name,
            &context.hash,
            |settings| {
                settings.asset_usage =
                    RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD;
                settings.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
                    address_mode_u: ImageAddressMode::Repeat,
                    address_mode_v: ImageAddressMode::ClampToEdge,
                    ..ImageSamplerDescriptor::linear()
                });
            },
        )
        .ok()
    });
    if slot.source.is_some() {
        next.texture_state = "loading";
        if let Some(handle) = &slot.handle {
            if let Some(image) = images.get(handle) {
                if valid_sky_image(image) {
                    next.texture = Some(handle.clone());
                    next.texture_state = "ready";
                } else {
                    next.texture_state = "invalid aspect ratio";
                }
            } else if matches!(
                server.load_state(handle.id()),
                bevy::asset::LoadState::Failed(_)
            ) {
                next.texture_state = "failed";
            }
        } else {
            next.texture_state = "unresolved";
        }
    }
    state.set_if_neq(next);
}

pub(super) type SceneSkyMaterial = ExtendedMaterial<SceneSkyExtension>;

#[derive(Asset, TypePath, AsBindGroup, Debug, Clone)]
pub(super) struct SceneSkyExtension {
    #[uniform(100)]
    params: SceneSkyUniform,
    #[texture(101)]
    #[sampler(102)]
    image: Option<Handle<Image>>,
}

mod uniform {
    // ShaderType emits layout assertion functions which are not called from Rust.
    #![allow(dead_code)]
    use super::*;

    #[derive(Debug, Clone, ShaderType)]
    pub(super) struct SceneSkyUniform {
        pub sun_direction: Vec3,
        pub has_image: u32,
        pub moon_direction: Vec3,
        pub visible: u32,
        pub sun_color: Vec3,
        pub night: f32,
    }
}
use uniform::SceneSkyUniform;

impl MaterialExtension for SceneSkyExtension {
    type Base = SkyBoxMaterial;
    fn fragment_shader() -> ShaderRef {
        "embedded://shaders/scene_sky.wgsl".into()
    }
}

fn update_sky_material(
    mut commands: Commands,
    state: Res<SceneEnvironment>,
    atmosphere: bevy_atmosphere::system_param::AtmosphereMut<crate::nishita_cloud::NishitaCloud>,
    bases: Res<Assets<SkyBoxMaterial>>,
    mut materials: ResMut<Assets<SceneSkyMaterial>>,
    new_boxes: Query<(Entity, &MeshMaterial3d<SkyBoxMaterial>)>,
    mut active: Local<Option<(Handle<SkyBoxMaterial>, Handle<SceneSkyMaterial>)>>,
) {
    for (entity, base_handle) in &new_boxes {
        let Some(base) = bases.get(&base_handle.0) else {
            continue;
        };
        let (_, handle) = active.get_or_insert_with(|| {
            (
                base_handle.0.clone(),
                materials.add(SceneSkyMaterial {
                    base: base.clone(),
                    extension: SceneSkyExtension {
                        params: SceneSkyUniform {
                            sun_direction: Vec3::Y,
                            moon_direction: Vec3::Y,
                            sun_color: Vec3::ONE,
                            night: 0.0,
                            has_image: 0,
                            visible: 1,
                        },
                        image: None,
                    },
                }),
            )
        });
        commands
            .entity(entity)
            .remove::<MeshMaterial3d<SkyBoxMaterial>>()
            .insert(MeshMaterial3d(handle.clone()));
    }
    if let Some((base, material)) = active
        .as_ref()
        .and_then(|(base, material)| materials.get_mut(material).map(|m| (base, m)))
    {
        if let Some(base) = bases.get(base) {
            material.base = base.clone();
        }
        material.extension.image = state.texture.clone();
        material.extension.params = SceneSkyUniform {
            has_image: u32::from(state.texture.is_some()),
            visible: u32::from(state.sun_visible.unwrap_or(true)),
            sun_direction: atmosphere.sun_position,
            moon_direction: atmosphere.moon_position,
            sun_color: if atmosphere.dir_light_intensity > 0.0 {
                atmosphere.sun_color
            } else {
                Vec3::ZERO
            },
            night: {
                let t = ((-atmosphere.sun_position.normalize_or_zero().y) / 0.15).clamp(0.0, 1.0);
                t * t * (3.0 - 2.0 * t)
            },
        };
    }
}

#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/scene_environment")]
struct EnvironmentCommand;

fn environment_command(
    mut command: ConsoleCommand<EnvironmentCommand>,
    state: Res<SceneEnvironment>,
) {
    let Some(Ok(_)) = command.take() else { return };
    command.reply_ok(
        serde_json::json!({
            "scene": state.source.map(|e| e.to_bits().to_string()),
            "textureState": if state.texture_state.is_empty() { "unset" } else { state.texture_state },
            "sunVisible": state.sun_visible.unwrap_or(true),
            "sunIntensity": state.sun_intensity.unwrap_or(1.0),
            "sunDirection": state.sun_direction.map(|c| c.to_array()),
            "sunColor": state.sun_color.map(|c| c.to_array()),
            "fogDensity": state.fog_density,
            "zenith": state.zenith.map(|c| c.to_array()),
            "horizon": state.horizon.map(|c| c.to_array()),
            "nadir": state.nadir.map(|c| c.to_array()),
            "rim": state.rim.map(|c| c.to_array()),
            "fogColor": state.fog_color.map(|c| c.to_array()),
            "cloudColor": state.cloud_color.map(|c| c.to_array()),
            "cloudSpeed": state.cloud_speed,
            "cloudOpacity": state.cloud_opacity,
            "starsBrightness": state.star_brightness,
        })
        .to_string(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use dcl_component::proto_components::{
        common::{Color4, ColorKey, Vector3},
        sdk::components::pb_skybox,
    };

    fn gradient(keys: &[(f32, f32)]) -> ColorGradient {
        ColorGradient {
            keys: keys
                .iter()
                .map(|&(time, red)| ColorKey {
                    time,
                    color: Some(Color4 {
                        r: red,
                        g: 0.0,
                        b: 0.0,
                        a: 1.0,
                    }),
                })
                .collect(),
        }
    }

    #[test]
    fn gradients_sort_clamp_keep_hdr_and_later_duplicates_without_midnight_wrap() {
        let colors = gradient(&[(1.5, 5.0), (0.5, 2.0), (-1.0, 1.0), (0.5, 3.0)]);
        assert_eq!(sample_gradient(Some(&colors), 0.0), Some(Vec3::X));
        assert_eq!(sample_gradient(Some(&colors), 0.25), Some(Vec3::X * 2.0));
        assert_eq!(sample_gradient(Some(&colors), 0.5), Some(Vec3::X * 3.0));
        assert_eq!(sample_gradient(Some(&colors), 1.0), Some(Vec3::X * 5.0));
        assert_eq!(sample_gradient(Some(&gradient(&[])), 0.5), None);
        assert_eq!(
            sample_gradient(
                Some(&gradient(&[(f32::NAN, 1.0), (0.0, f32::INFINITY)])),
                0.5
            ),
            None
        );
    }

    #[test]
    fn gradient_work_is_bounded_and_resamples_more_than_eight_keys() {
        let mut keys: Vec<_> = (0..64).map(|n| (n as f32 / 63.0, n as f32)).collect();
        keys.push((1.0, 999.0));
        assert_eq!(
            sample_gradient(Some(&gradient(&keys)), 1.0),
            Some(Vec3::X * 63.0)
        );
        assert!((sample_gradient(Some(&gradient(&keys)), 0.5).unwrap().x - 31.5).abs() < 1e-4);
    }

    #[test]
    fn optional_zero_false_negative_speed_and_scene_direction_are_not_defaults() {
        let sky = PbSkybox {
            clouds: Some(pb_skybox::Clouds {
                opacity: Some(0.0),
                speed: Some(-0.01),
                ..default()
            }),
            stars: Some(pb_skybox::Stars {
                brightness: Some(0.0),
            }),
            ..default()
        };
        let sun = PbSun {
            intensity: Some(0.0),
            visible: Some(false),
            direction: Some(Vector3 {
                x: 0.0,
                y: 0.0,
                z: 4.0,
            }),
            ..default()
        };
        let fog = PbEnvironmentFog {
            density: Some(0.0),
            ..default()
        };
        let transform = GlobalTransform::from(Transform::from_rotation(Quat::from_rotation_y(
            std::f32::consts::FRAC_PI_2,
        )));
        let state = sample_components(
            Entity::PLACEHOLDER,
            Some(&sky),
            Some(&sun),
            Some(&fog),
            &transform,
            0.5,
        );
        assert_eq!(state.sun_visible, Some(false));
        assert_eq!(state.sun_intensity, Some(0.0));
        assert_eq!(state.cloud_opacity, Some(0.0));
        assert_eq!(state.cloud_speed, Some(-0.01));
        assert_eq!(state.star_brightness, Some(0.0));
        assert_eq!(state.fog_density, Some(0.0));
        assert!(state.sun_direction.unwrap().abs_diff_eq(Vec3::NEG_X, 1e-6));
        let empty = sample_components(Entity::PLACEHOLDER, None, None, None, &transform, 0.5);
        assert_eq!(empty.sun_intensity, None);
        assert_eq!(empty.cloud_opacity, None);
        assert_eq!(empty.fog_density, None);
    }

    #[test]
    fn invalid_numbers_and_zero_direction_leave_defaults_and_rim_follows_horizon() {
        let sky = PbSkybox {
            sky_colors: Some(pb_skybox::SkyColors {
                horizon: Some(gradient(&[(0.0, 4.0)])),
                ..default()
            }),
            ..default()
        };
        let sun = PbSun {
            direction: Some(Vector3::default()),
            intensity: Some(f32::INFINITY),
            ..default()
        };
        let fog = PbEnvironmentFog {
            density: Some(-1.0),
            ..default()
        };
        let state = sample_components(
            Entity::PLACEHOLDER,
            Some(&sky),
            Some(&sun),
            Some(&fog),
            &GlobalTransform::IDENTITY,
            0.5,
        );
        assert_eq!(state.sun_intensity, None);
        assert_eq!(state.sun_direction, None);
        assert_eq!(state.fog_density, Some(0.0));
        assert_eq!(state.rim, state.horizon);
        assert_eq!(state.horizon, Some(Vec3::X * 4.0));
    }

    #[test]
    fn changing_scene_or_removing_texture_discards_pending_and_failed_requests() {
        let mut world = World::new();
        let scene_a = world.spawn_empty().id();
        let scene_b = world.spawn_empty().id();
        let mut images = Assets::<Image>::default();
        let old = images.add(Image::default());
        let new = images.add(Image::default());
        let mut slot = TextureSlot::default();
        slot.select(
            Some((scene_a, "deployment-a".into(), "sky.png".into())),
            |_| Some(old.clone()),
        );
        slot.select(
            Some((scene_a, "deployment-a".into(), "sky.png".into())),
            |_| panic!("same request must not restart"),
        );
        slot.select(
            Some((scene_b, "deployment-b".into(), "sky.png".into())),
            |_| Some(new.clone()),
        );
        assert_eq!(slot.handle, Some(new));
        // Completing the old image cannot replace the selected scene's handle.
        images.insert(old.id(), Image::default());
        assert_ne!(slot.handle, Some(old));
        // The same scene entity and file name in a new deployment is a new request.
        let mut reloaded = false;
        slot.select(
            Some((scene_b, "deployment-c".into(), "sky.png".into())),
            |_| {
                reloaded = true;
                None
            },
        );
        assert!(reloaded && slot.handle.is_none());
        slot.select(
            Some((scene_b, "deployment-b".into(), "missing.png".into())),
            |_| None,
        );
        assert!(slot.handle.is_none());
        slot.select(None, |_| panic!("clearing must not load"));
        assert!(slot.handle.is_none() && slot.source.is_none());
    }

    #[test]
    fn texture_layout_requires_equirectangular_two_to_one() {
        let mut image = Image::default();
        image.resize(bevy::render::render_resource::Extent3d {
            width: 4,
            height: 2,
            depth_or_array_layers: 1,
        });
        assert!(valid_sky_image(&image));
        image.resize(bevy::render::render_resource::Extent3d {
            width: 3,
            height: 2,
            depth_or_array_layers: 1,
        });
        assert!(!valid_sky_image(&image));
    }

    #[test]
    fn light_fog_and_shader_overrides_reset_without_compounding_or_bypassing_quality() {
        use crate::{
            nishita_cloud::NishitaCloud, sky_envmap::SkyEnvmapSettings, CloudCover,
            ShadowBiasOverride,
        };
        use bevy_atmosphere::prelude::AtmosphereModel;
        use common::structs::{AppConfig, FogSetting, SceneGlobalLight, SceneLoadDistance};
        let mut app = App::new();
        let mut time = Time::<()>::default();
        time.advance_by(std::time::Duration::from_secs(1));
        let mut config = AppConfig::default();
        config.graphics.fog = FogSetting::Atmospheric;
        app.insert_resource(time)
            .insert_resource(config)
            .insert_resource(TimeOfDay { time: 43200.0 })
            .insert_resource(SceneLoadDistance {
                load: 100.0,
                unload: 20.0,
                load_imposter: 0.0,
            })
            .insert_resource(SceneGlobalLight {
                dir_color: Color::WHITE,
                dir_illuminance: 1000.0,
                sun_illuminance: 1000.0,
                dir_direction: Vec3::NEG_Y,
                sun_direction: Vec3::NEG_Y,
                ambient_color: Color::WHITE,
                ambient_brightness: 1.0,
                ..default()
            })
            .insert_resource(AtmosphereModel::new(NishitaCloud::default()))
            .insert_resource(CloudCover {
                cover: 0.375,
                speed: 5.0,
                density_cap: 0.8,
                shadow: 0.05,
                scale: 1.5,
                steps: 44,
                lacunarity: 2.0,
                sharpness: 50.0,
                sharp_mid: 0.1,
            })
            .init_resource::<AmbientLight>()
            .init_resource::<SkyEnvmapSettings>()
            .init_resource::<ShadowBiasOverride>()
            .insert_resource(SceneEnvironment {
                sun_intensity: Some(2.0),
                sun_visible: Some(false),
                fog_density: Some(0.0),
                zenith: Some(Vec3::X * 2.0),
                nadir: Some(Vec3::Z),
                cloud_opacity: Some(0.0),
                ..default()
            })
            .add_systems(Update, crate::apply_global_light);
        let camera = app
            .world_mut()
            .spawn((Camera3d::default(), DistanceFog::default()))
            .id();
        for _ in 0..3 {
            app.update();
        }
        let world = app.world_mut();
        assert_eq!(
            world
                .query::<&DirectionalLight>()
                .single(world)
                .unwrap()
                .illuminance,
            2000.0
        );
        assert!(matches!(
            world.get::<DistanceFog>(camera).unwrap().falloff,
            FogFalloff::Exponential { density: 0.0 }
        ));
        let atmosphere = world
            .resource::<AtmosphereModel>()
            .to_ref::<NishitaCloud>()
            .unwrap();
        assert_eq!(atmosphere.scene_zenith, Vec4::new(2.0, 0.0, 0.0, 1.0));
        assert_eq!(atmosphere.scene_sun_visible, 0.0);
        let ambient = world.resource::<AmbientLight>();
        assert!(ambient.color.to_linear().alpha * ambient.brightness < -1.5);
        world.resource_mut::<AppConfig>().graphics.fog = FogSetting::Off;
        app.update();
        assert!(!matches!(
            app.world().get::<DistanceFog>(camera).unwrap().falloff,
            FogFalloff::Exponential { .. }
        ));
        *app.world_mut().resource_mut::<SceneEnvironment>() = SceneEnvironment::default();
        app.update();
        let world = app.world_mut();
        assert_eq!(
            world
                .query::<&DirectionalLight>()
                .single(world)
                .unwrap()
                .illuminance,
            1000.0
        );
        let atmosphere = world
            .resource::<AtmosphereModel>()
            .to_ref::<NishitaCloud>()
            .unwrap();
        assert_eq!(atmosphere.scene_zenith, Vec4::ZERO);
        assert_eq!(atmosphere.scene_sun_visible, 1.0);
        assert_eq!(atmosphere.scene_cloud_opacity, 1.0);
        let ambient = world.resource::<AmbientLight>();
        assert!(ambient.color.to_linear().alpha * ambient.brightness >= -1.0);
    }

    /// Compiles both the procedural compute shader and the visible image material on a GPU.
    #[test]
    #[ignore = "needs a GPU; run with --ignored"]
    fn scene_environment_pipelines_compile() {
        use crate::nishita_cloud::{init_noise, NishitaCloud};
        use bevy::{
            core_pipeline::tonemapping::Tonemapping,
            render::{
                camera::RenderTarget,
                gpu_readback::{Readback, ReadbackComplete},
                render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages},
                view::screenshot::{Screenshot, ScreenshotCaptured},
                RenderApp,
            },
        };
        use pipeline_test::{
            compile_pipelines, fragment_pipelines, headless_app_without_embedded_assets,
            render_target,
        };
        #[derive(Resource, Default)]
        struct Captured(Option<Image>);
        #[derive(Resource, Default)]
        struct RawCaptured(Option<Vec<u8>>);
        fn raw_cube(
            app: &mut App,
            handle: &Handle<Image>,
            size: usize,
            label: &str,
        ) -> Vec<[u16; 4]> {
            app.world_mut().resource_mut::<RawCaptured>().0 = None;
            let entity = app
                .world_mut()
                .spawn(Readback::texture(handle.clone()))
                .observe(
                    |event: Trigger<ReadbackComplete>, mut captured: ResMut<RawCaptured>| {
                        captured.0 = Some(event.event().0.clone());
                    },
                )
                .id();
            let start = std::time::Instant::now();
            let bytes = loop {
                app.update();
                if let Some(bytes) = app.world_mut().resource_mut::<RawCaptured>().0.take() {
                    break bytes;
                }
                assert!(
                    start.elapsed().as_secs() < 30,
                    "{label}: raw GPU readback timed out"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            app.world_mut().despawn(entity);
            // Readback rows are aligned to 256 bytes; skip padding, retain every face/texel/channel.
            let stride = (size * 8).div_ceil(256) * 256;
            assert_eq!(bytes.len(), stride * size * 6, "{label}: six cubemap faces");
            let mut texels = Vec::with_capacity(size * size * 6);
            for face in 0..6 {
                for y in 0..size {
                    for x in 0..size {
                        let offset = (face * size + y) * stride + x * 8;
                        let channels = std::array::from_fn(|channel| {
                            let i = offset + channel * 2;
                            u16::from_le_bytes([bytes[i], bytes[i + 1]])
                        });
                        for value in channels {
                            assert_ne!(
                                value & 0x7c00,
                                0x7c00,
                                "{label}: nonfinite f16 face={face} x={x} y={y}: {channels:?}"
                            );
                        }
                        texels.push(channels);
                    }
                }
            }
            texels
        }
        fn tick(mut atmosphere: bevy_atmosphere::system_param::AtmosphereMut<NishitaCloud>) {
            atmosphere.tick = atmosphere.tick.wrapping_add(1);
        }
        fn capture(app: &mut App, target: &Handle<Image>, label: &str) -> [u8; 4] {
            app.world_mut().resource_mut::<Captured>().0 = None;
            app.world_mut()
                .spawn(Screenshot::image(target.clone()))
                .observe(
                    |event: Trigger<ScreenshotCaptured>, mut captured: ResMut<Captured>| {
                        captured.0 = Some(event.event().0.clone())
                    },
                );
            let start = std::time::Instant::now();
            loop {
                app.update();
                if let Some(image) = app.world_mut().resource_mut::<Captured>().0.take() {
                    let rgba = image
                        .try_into_dynamic()
                        .expect("captured RGBA image")
                        .to_rgba8();
                    if let Some(directory) = std::env::var_os("DCL_ENVIRONMENT_SCREENSHOTS") {
                        std::fs::create_dir_all(&directory).unwrap();
                        rgba.save(std::path::Path::new(&directory).join(format!("{label}.png")))
                            .unwrap();
                    }
                    return rgba.get_pixel(32, 32).0;
                }
                assert!(start.elapsed().as_secs() < 30, "GPU screenshot timed out");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        use bevy_atmosphere::{
            model::AddAtmosphereModel, pipeline::AtmosphereImageBindGroupLayout, prelude::*,
        };
        let mut app = headless_app_without_embedded_assets();
        app.init_resource::<SceneEnvironment>()
            .init_resource::<Captured>()
            .init_resource::<RawCaptured>()
            .init_resource::<common::structs::AppConfig>()
            .add_plugins(console::ConsolePlugin {
                add_bevy_console: false,
            })
            .insert_resource(AtmosphereSettings {
                resolution: 128,
                ..default()
            })
            .insert_resource(AtmosphereModel::new(NishitaCloud::default()))
            .add_plugins(AtmospherePlugin)
            .add_plugins(crate::sky_envmap::SkyEnvmapPlugin)
            .add_plugins(MaterialPlugin::<SceneSkyMaterial> {
                prepass_enabled: false,
                shadows_enabled: false,
                ..default()
            })
            .add_systems(Update, (tick, update_sky_material).chain())
            .add_plugins(assets::EmbedAssetsPlugin);
        app.finish();
        app.cleanup();
        app.sub_app_mut(RenderApp)
            .init_resource::<AtmosphereImageBindGroupLayout>();
        app.add_atmosphere_model::<NishitaCloud>();
        let raw_sky = app
            .world()
            .resource::<bevy_atmosphere::pipeline::AtmosphereImage>()
            .handle
            .clone();
        let maps = app
            .world()
            .resource::<crate::sky_envmap::SkyEnvmap>()
            .clone();
        for handle in [
            &raw_sky,
            &maps.diffuse,
            &maps.specular,
            &maps.trilight_diffuse,
            &maps.trilight_specular,
        ] {
            app.world_mut()
                .resource_mut::<Assets<Image>>()
                .get_mut(handle)
                .unwrap()
                .texture_descriptor
                .usage |= TextureUsages::COPY_SRC;
        }
        let noise = app
            .world_mut()
            .resource_mut::<Assets<Image>>()
            .add(init_noise(16));
        // Centre columns face DCL +Z; the seam faces -Z.
        let mut sky_image = Image::new(
            Extent3d {
                width: 4,
                height: 2,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            [
                [0, 255, 0, 255],
                [255, 0, 0, 255],
                [255, 0, 0, 255],
                [0, 255, 0, 255],
            ]
            .repeat(2)
            .into_iter()
            .flatten()
            .collect(),
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::all(),
        );
        sky_image.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
            address_mode_u: ImageAddressMode::Repeat,
            ..ImageSamplerDescriptor::linear()
        });
        let sky = app
            .world_mut()
            .resource_mut::<Assets<Image>>()
            .add(sky_image);
        app.world_mut().resource_mut::<SceneEnvironment>().texture = Some(sky);
        app.world_mut().resource_mut::<SceneEnvironment>().nadir = Some(Vec3::Z);
        app.world_mut()
            .resource_mut::<AtmosphereModel>()
            .to_mut::<NishitaCloud>()
            .unwrap()
            .noise_texture = noise;
        let target = render_target(&mut app);
        let camera = app
            .world_mut()
            .spawn((
                Camera3d::default(),
                Tonemapping::None,
                Camera {
                    target: RenderTarget::Image(target.clone().into()),
                    ..default()
                },
                AtmosphereCamera::default(),
                Transform::default(),
            ))
            .id();
        for _ in 0..50 {
            app.update();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        {
            use bevy::render::render_resource::{
                CachedPipelineState, PipelineCache, PipelineDescriptor,
            };
            let cache = app.sub_app(RenderApp).world().resource::<PipelineCache>();
            for pipeline in cache.pipelines() {
                if matches!(pipeline.state, CachedPipelineState::Ok(_)) {
                    continue;
                }
                let label = match &pipeline.descriptor {
                    PipelineDescriptor::RenderPipelineDescriptor(p) => &p.label,
                    PipelineDescriptor::ComputePipelineDescriptor(p) => &p.label,
                };
                eprintln!(
                    "pending environment pipeline {label:?}: {:?}",
                    pipeline.state
                );
            }
        }
        assert!(compile_pipelines(&mut app) > 0);
        assert!(fragment_pipelines(&app, "embedded://shaders/scene_sky.wgsl") > 0);
        let front = capture(&mut app, &target, "texture-forward");
        assert!(
            front[0] > 200 && front[1] < 30 && front[2] < 30,
            "centre must face DCL +Z: {front:?}"
        );
        app.world_mut()
            .get_mut::<Transform>(camera)
            .unwrap()
            .rotation = Quat::from_rotation_y(std::f32::consts::PI);
        let back = capture(&mut app, &target, "texture-backward");
        assert!(
            back[1] > 200 && back[0] < 30 && back[2] < 30,
            "seam must face DCL -Z: {back:?}"
        );
        app.world_mut().resource_mut::<SceneEnvironment>().texture = None;
        {
            let mut model = app.world_mut().resource_mut::<AtmosphereModel>();
            let atmosphere = model.to_mut::<NishitaCloud>().unwrap();
            atmosphere.scene_zenith = Vec4::new(0.0, 0.0, 1.0, 1.0);
            atmosphere.scene_horizon = atmosphere.scene_zenith;
            atmosphere.scene_nadir = atmosphere.scene_zenith;
            atmosphere.scene_cloud_opacity = 0.0;
            atmosphere.scene_sun_visible = 0.0;
        }
        for _ in 0..80 {
            app.update();
        }
        let restored = capture(&mut app, &target, "procedural-restored");
        assert!(
            restored[2] > 200 && restored[0] < 30 && restored[1] < 30,
            "removing image restores authored procedural sky: {restored:?}"
        );
        // Nonzero daylight evaluates the procedural fallback even at an authored lower pole.
        // Horizontal screenshots and a zero light intensity cannot detect a NaN at face -Y.
        for band in ["nadir", "zenith", "horizon", "rim", "none"] {
            {
                let mut model = app.world_mut().resource_mut::<AtmosphereModel>();
                let atmosphere = model.to_mut::<NishitaCloud>().unwrap();
                atmosphere.dir_light_intensity = 11000.0;
                atmosphere.sun_intensity = 22.0;
                atmosphere.sun_position = Vec3::Y;
                atmosphere.scene_cloud_opacity = 0.0;
                atmosphere.scene_sun_visible = 0.0;
                atmosphere.scene_zenith = Vec4::ZERO;
                atmosphere.scene_horizon = Vec4::ZERO;
                atmosphere.scene_nadir = Vec4::ZERO;
                atmosphere.scene_rim = Vec4::ZERO;
                let red = Vec4::new(1.0, 0.0, 0.0, 1.0);
                match band {
                    "nadir" => atmosphere.scene_nadir = red,
                    "zenith" => atmosphere.scene_zenith = red,
                    "horizon" => atmosphere.scene_horizon = red,
                    "rim" => atmosphere.scene_rim = red,
                    _ => (),
                }
            }
            app.world_mut().resource_mut::<SceneEnvironment>().nadir =
                (band == "nadir").then_some(Vec3::X);
            for quality in [
                common::structs::SkyReflectionSetting::High,
                common::structs::SkyReflectionSetting::Low,
            ] {
                app.world_mut()
                    .resource_mut::<common::structs::AppConfig>()
                    .graphics
                    .sky_reflections = quality;
                for _ in 0..80 {
                    app.update();
                }
                let raw = raw_cube(&mut app, &raw_sky, 128, band);
                let pole = raw[3 * 128 * 128 + 64 * 128 + 64];
                let expected_red = if band == "nadir" { 0x3c00 } else { 0 };
                assert_eq!(
                    pole.map(|c| c & 0x7fff),
                    [expected_red, 0, 0, 0],
                    "{band}: exact raw -Y pole"
                );
                let handles = match quality {
                    common::structs::SkyReflectionSetting::High => [&maps.diffuse, &maps.specular],
                    common::structs::SkyReflectionSetting::Low => {
                        [&maps.trilight_diffuse, &maps.trilight_specular]
                    }
                };
                for handle in handles {
                    let size = app
                        .world()
                        .resource::<Assets<Image>>()
                        .get(handle)
                        .unwrap()
                        .width() as usize;
                    raw_cube(
                        &mut app,
                        handle,
                        size,
                        &format!("{band} {quality:?} ambient"),
                    );
                }
            }
        }
    }
}
