mod atmosphere_params;
mod coast;
mod day_night;
mod debug_panel;
mod environment;
mod nishita_cloud;
mod props;
pub mod shell_texturing;
mod sky_envmap;
mod solids;
mod stars;
pub mod terrain;
mod trees;

use bevy::{
    core_pipeline::dof::{DepthOfField, DepthOfFieldMode},
    pbr::{wireframe::WireframePlugin, DirectionalLightShadowMap},
    prelude::*,
    render::view::{Layer, RenderLayers},
};

use bevy::render::{Render, RenderApp, RenderSet};
use bevy_atmosphere::{
    model::AddAtmosphereModel,
    pipeline::{AtmosphereImageBindGroupLayout, AtmosphereUpdateEvent},
    prelude::{AtmosphereCamera, AtmosphereModel, AtmospherePlugin, AtmosphereSettings},
    system_param::AtmosphereMut,
};
use nishita_cloud::{init_noise, NishitaCloud};

use bevy_console::ConsoleCommand;
use common::{
    sets::{SceneSets, SetupSets},
    structs::{
        sun_cascades, AppConfig, DofConfig, FogSetting, PrimaryCamera, PrimaryCameraRes,
        PrimaryUser, SceneGlobalLight, SceneLoadDistance, TimeOfDay, PRIMARY_AVATAR_LIGHT_LAYER,
    },
};
use console::DoAddConsoleCommand;
use scene_runner::update_world::lights::{moon_direction, SUN_COLOR_NOON};

use crate::{
    coast::CoastPlugin,
    day_night::DayNightPlugin,
    props::PropsPlugin,
    shell_texturing::ShellTexturingPlugin,
    sky_envmap::{SkyEnvmapPlugin, SkyEnvmapSettings},
    solids::SolidsPlugin,
    terrain::TerrainPlugin,
    trees::TreesPlugin,
};

pub struct VisualsPlugin {
    pub no_fog: bool,
}

impl Plugin for VisualsPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(debug_panel::VisualsDebugPanelPlugin);
        app.insert_resource(DirectionalLightShadowMap { size: 4096 })
            .init_resource::<SceneGlobalLight>()
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
            .add_plugins(WireframePlugin::default())
            .add_plugins(DayNightPlugin)
            .add_plugins(ShellTexturingPlugin)
            .add_plugins(TerrainPlugin)
            .add_plugins(CoastPlugin)
            .add_plugins((TreesPlugin, PropsPlugin, SolidsPlugin))
            .add_systems(
                Update,
                apply_global_light
                    .in_set(SceneSets::PostLoop)
                    .after(scene_runner::update_world::lights::update_directional_light),
            )
            .add_systems(Update, update_dof)
            .add_systems(Startup, setup.in_set(SetupSets::Main));

        app.insert_resource(AtmosphereSettings {
            resolution: 1024,
            dithering: true,
            skybox_creation_mode:
                bevy_atmosphere::settings::SkyboxCreationMode::FromProjectionFarWithFallback(
                    99999.0,
                ),
        })
        .insert_resource(AtmosphereModel::new(NishitaCloud::default()))
        .add_plugins(AtmospherePlugin)
        .add_plugins(environment::SceneEnvironmentPlugin)
        .add_plugins(SkyEnvmapPlugin)
        .add_plugins(stars::StarsPlugin);

        app.add_console_command::<ShadowConsoleCommand, _>(shadow_console_command);
        app.init_resource::<ShadowBiasOverride>();
        app.add_console_command::<ShadowBiasConsoleCommand, _>(shadow_bias_console_command);
        app.add_console_command::<FogConsoleCommand, _>(fog_console_command);
        app.add_console_command::<DofConsoleCommand, _>(dof_console_command);
        app.add_console_command::<CloudConsoleCommand, _>(cloud_console_command);
        app.add_console_command::<CloudDensityConsoleCommand, _>(cloud_density_console_command);
        app.add_console_command::<CloudShadowConsoleCommand, _>(cloud_shadow_console_command);
        app.add_console_command::<CloudScaleConsoleCommand, _>(cloud_scale_console_command);
        app.add_console_command::<CloudStepsConsoleCommand, _>(cloud_steps_console_command);
        app.add_console_command::<CloudLacunarityConsoleCommand, _>(
            cloud_lacunarity_console_command,
        );
        app.add_console_command::<CloudSharpConsoleCommand, _>(cloud_sharp_console_command);
        app.add_console_command::<TonemapConsoleCommand, _>(tonemap_console_command);
        app.add_console_command::<ExposureConsoleCommand, _>(exposure_console_command);
        app.add_console_command::<GammaConsoleCommand, _>(gamma_console_command);
        app.add_console_command::<SaturationConsoleCommand, _>(saturation_console_command);
        app.add_console_command::<BloomConsoleCommand, _>(bloom_console_command);
    }

    fn finish(&self, app: &mut App) {
        let snapshots_only = app
            .world()
            .resource::<scene_runner::initialize_scene::TestingData>()
            .test_scenes
            .is_some();
        let render_app = app.sub_app_mut(RenderApp);
        render_app.init_resource::<AtmosphereImageBindGroupLayout>();
        if snapshots_only {
            // The player camera is inactive and snapshot cameras have no sky or
            // environment map. Do not raymarch an unused 1024px sky cubemap every
            // frame; scene lighting and snapshot rendering still run normally.
            render_app.add_systems(
                Render,
                (|mut updates: ResMut<Events<AtmosphereUpdateEvent>>| updates.clear())
                    .in_set(RenderSet::PrepareBindGroups),
            );
        }

        app.add_atmosphere_model::<NishitaCloud>();
    }
}

#[derive(Component)]
struct DirectionalLightLayer(Layer);

fn setup(
    mut commands: Commands,
    camera: Res<PrimaryCameraRes>,
    mut atmosphere: AtmosphereMut<NishitaCloud>,
    mut images: ResMut<Assets<Image>>,
) {
    info!("visuals::setup");

    commands.entity(camera.0).try_insert(DistanceFog {
        color: Color::srgb(0.3, 0.2, 0.1),
        directional_light_color: Color::srgb(1.0, 1.0, 0.7),
        directional_light_exponent: 10.0,
        falloff: FogFalloff::ExponentialSquared { density: 0.01 },
    });

    {
        commands.entity(camera.0).try_insert(AtmosphereCamera {
            render_layers: Some(RenderLayers::default()),
        });

        let noise = init_noise(512);
        let h_noise = images.add(noise);

        atmosphere.noise_texture = h_noise;
    }
}

static TRANSITION_TIME: f32 = 1.0;

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn apply_global_light(
    mut commands: Commands,
    setting: Res<AppConfig>,
    mut atmosphere: AtmosphereMut<NishitaCloud>,
    cloud: Res<CloudCover>,
    mut sun: Query<(
        Entity,
        &DirectionalLightLayer,
        &mut Transform,
        &mut DirectionalLight,
    )>,
    mut ambient: ResMut<AmbientLight>,
    time: Res<Time>,
    mut cameras: Query<(Option<&PrimaryCamera>, Option<&mut DistanceFog>), With<Camera3d>>,
    scene_distance: Res<SceneLoadDistance>,
    scene_light: (Res<SceneGlobalLight>, Res<environment::SceneEnvironment>),
    time_of_day: Res<TimeOfDay>,
    envmap_settings: Res<SkyEnvmapSettings>,
    shadow_bias: Res<ShadowBiasOverride>,
    mut prev: Local<(f32, SceneGlobalLight)>,
    mut cloud_dt: Local<f32>,
    mut last_primary_distance: Local<f32>,
) {
    let (scene_global_light, environment) = scene_light;
    // the transition has settled once the previous output exactly matches the target
    let settled = prev.0 >= TRANSITION_TIME && prev.1 == *scene_global_light;

    let mut next_light = if prev.0 >= TRANSITION_TIME && prev.1.source == scene_global_light.source
    {
        scene_global_light.clone()
    } else {
        // transition part way
        let new_amount = if prev.1.source == scene_global_light.source {
            (time.delta_secs() / (TRANSITION_TIME - prev.0)).clamp(0.0, 1.0)
        } else {
            time.delta_secs() / TRANSITION_TIME
        };
        let old_amount = 1.0 - new_amount;
        SceneGlobalLight {
            source: scene_global_light.source,
            dir_color: (scene_global_light.dir_color.to_srgba() * new_amount
                + prev.1.dir_color.to_srgba() * old_amount)
                .into(),
            dir_illuminance: scene_global_light.dir_illuminance * new_amount
                + prev.1.dir_illuminance * old_amount,
            dir_direction: prev
                .1
                .dir_direction
                .lerp(scene_global_light.dir_direction, new_amount),
            sun_illuminance: scene_global_light.sun_illuminance * new_amount
                + prev.1.sun_illuminance * old_amount,
            sun_direction: prev
                .1
                .sun_direction
                .lerp(scene_global_light.sun_direction, new_amount),
            ambient_color: (scene_global_light.ambient_color.to_srgba() * new_amount
                + prev.1.ambient_color.to_srgba() * old_amount)
                .into(),
            ambient_brightness: scene_global_light.ambient_brightness * new_amount
                + prev.1.ambient_brightness * old_amount,
            layers: scene_global_light.layers.clone(),
        }
    };

    // Keep the transition history free of scene multipliers so they never compound.
    let base_light = next_light.clone();
    if let Some(color) = environment.sun_color {
        next_light.dir_color = Color::linear_rgb(color.x, color.y, color.z);
    }
    if let Some(intensity) = environment.sun_intensity {
        next_light.dir_illuminance *= intensity;
        next_light.sun_illuminance *= intensity;
    }
    if let Some(direction) = environment.sun_direction {
        next_light.dir_direction = direction;
        next_light.sun_direction = direction;
    }
    let rotation = Quat::from_rotation_arc(Vec3::NEG_Z, next_light.dir_direction);

    // physically-simulated sky: the rayleigh coefficient (hue) follows the light colour, so a
    // scene's light colour changes the sky; the mie coefficient (haze) rises with the sun; the
    // sun sets naturally (no floor), and a flat night colour (added in-shader) provides the
    // night sky
    let sun_elevation = -next_light.sun_direction.y;
    let light_ratio = base_light.dir_color.to_srgba().to_vec3() / SUN_COLOR_NOON;
    atmosphere.sun_position = -next_light.sun_direction;
    atmosphere.rayleigh_coefficient = atmosphere_params::RAYLEIGH_NOON * light_ratio.powf(3.0);
    atmosphere.mie_coefficient = atmosphere_params::mie(sun_elevation);
    atmosphere.night_color = atmosphere_params::NIGHT_SKY;
    // the moon is drawn where the night light comes from
    atmosphere.moon_position = -moon_direction(time_of_day.elapsed_secs() / 3600.0);
    atmosphere.dir_light_intensity = next_light.sun_illuminance;
    atmosphere.sun_color = environment
        .sun_color
        .unwrap_or_else(|| next_light.dir_color.to_srgba().to_vec3());
    atmosphere.tick += 1;
    let band = |color: Option<Vec3>| color.map(|c| c.extend(1.0)).unwrap_or(Vec4::ZERO);
    atmosphere.scene_zenith = band(environment.zenith);
    atmosphere.scene_horizon = band(environment.horizon);
    atmosphere.scene_nadir = band(environment.nadir);
    atmosphere.scene_rim = band(environment.rim);
    let procedural_clouds = environment.texture.is_none();
    atmosphere.scene_cloud_color = band(environment.cloud_color.filter(|_| procedural_clouds));
    atmosphere.scene_cloud_opacity = environment
        .cloud_opacity
        .filter(|_| procedural_clouds)
        .unwrap_or(1.0);
    atmosphere.scene_sun_visible = if environment.sun_visible.unwrap_or(true) {
        1.0
    } else {
        0.0
    };
    if let Some(speed) = environment.cloud_speed.filter(|_| procedural_clouds) {
        atmosphere.scene_cloud_rotation = (atmosphere.scene_cloud_rotation as f64
            + speed as f64 * time.delta_secs_f64())
        .rem_euclid(std::f64::consts::TAU) as f32;
    } else {
        atmosphere.scene_cloud_rotation = 0.0;
    }

    if atmosphere.cloudy != cloud.cover {
        *cloud_dt = (*cloud_dt + time.delta_secs() * 20.0)
            .min(80.0 * (atmosphere.cloudy - cloud.cover).abs())
            .max(1.0);
        atmosphere.cloudy += (cloud.cover - atmosphere.cloudy).clamp(
            -time.delta_secs() * 0.005 * *cloud_dt,
            time.delta_secs() * 0.005 * *cloud_dt,
        );
        // atmosphere.time += time.delta_secs() * 10.0;
    } else {
        *cloud_dt = f32::max(*cloud_dt - time.delta_secs(), cloud.speed);
    }

    // An explicit angular speed controls the authored cloud layer, including a static zero.
    if environment.cloud_speed.is_none() || !procedural_clouds {
        atmosphere.time += time.delta_secs() * *cloud_dt;
    }

    // cloud look (live-tunable, baked later)
    atmosphere.cloud_density_cap = cloud.density_cap;
    atmosphere.cloud_shadow = cloud.shadow;
    atmosphere.cloud_scale = cloud.scale;
    atmosphere.cloud_steps = cloud.steps;
    atmosphere.cloud_lacunarity = cloud.lacunarity;
    atmosphere.cloud_sharpness = cloud.sharpness;
    atmosphere.cloud_sharp_mid = cloud.sharp_mid;

    // skip the light/fog/ambient writes (which trigger change detection and re-extraction)
    // when the light has settled and nothing else affecting them has changed
    let primary_distance = cameras
        .iter()
        .find_map(|(maybe_primary, _)| maybe_primary.map(|camera| camera.distance))
        .unwrap_or(0.0);
    // a (re)inserted DistanceFog fires `is_added` and must be written even when the light has
    // settled. reading the tick through the existing `&mut` access avoids the query conflict an
    // `Added<DistanceFog>` filter would have with `cameras`; `is_added` (not `is_changed`) is used
    // because our own per-frame fog writes set `changed`, which would otherwise never let the gate
    // re-engage.
    let fog_added = cameras
        .iter_mut()
        .any(|(_, fog)| fog.is_some_and(|fog| fog.is_added()));
    let skip_writes = settled
        && !environment.is_changed()
        && !setting.is_changed()
        && !envmap_settings.is_changed()
        && !shadow_bias.is_changed()
        && !scene_distance.is_changed()
        && !fog_added
        && *last_primary_distance == primary_distance;
    *last_primary_distance = primary_distance;

    if skip_writes {
        prev.0 += time.delta_secs();
        prev.1 = base_light;
        return;
    }

    let (shadow_depth_bias, shadow_normal_bias) = shadow_bias.0.unwrap_or(SUN_SHADOW_BIAS);
    let mut directional_layers = RenderLayers::none();
    for (entity, layer, mut light_trans, mut directional) in sun.iter_mut() {
        if !next_light.layers.intersects(&RenderLayers::layer(layer.0)) {
            commands.entity(entity).despawn();
            continue;
        }

        directional_layers = directional_layers.with(layer.0);
        light_trans.rotation = rotation;
        directional.illuminance = next_light.dir_illuminance;
        directional.color = next_light.dir_color;
        directional.shadow_depth_bias = shadow_depth_bias;
        directional.shadow_normal_bias = shadow_normal_bias;
    }

    for new_layer in next_light
        .layers
        .symmetric_difference(&directional_layers)
        .iter()
    {
        let mut layer = RenderLayers::layer(new_layer);
        if new_layer == 0 {
            layer = layer.union(&PRIMARY_AVATAR_LIGHT_LAYER);
        }

        let (shadows_enabled, cascade_shadow_config) = match sun_cascades(
            setting.graphics.shadow_settings,
            setting.graphics.shadow_distance,
        ) {
            Some(cascades) => (true, cascades),
            None => (false, Default::default()),
        };

        commands.spawn((
            DirectionalLight {
                color: next_light.dir_color,
                illuminance: next_light.dir_illuminance,
                shadows_enabled,
                shadow_depth_bias,
                shadow_normal_bias,
                ..Default::default()
            },
            Transform::default().with_rotation(rotation),
            cascade_shadow_config,
            layer,
            DirectionalLightLayer(new_layer),
        ));
    }

    for (maybe_primary, maybe_fog) in cameras.iter_mut() {
        let dir_light_lightness = Lcha::from(next_light.dir_color).lightness;
        // floor keeps night fog tinted instead of going black
        let skybox_brightness =
            (next_light.sun_illuminance.sqrt() * 40.0 * dir_light_lightness).clamp(400.0, 2000.0);

        if let Some(mut fog) = maybe_fog {
            let distance = (scene_distance.load + scene_distance.unload)
                .max(scene_distance.load_imposter * 0.333)
                + maybe_primary.map_or(0.0, |camera| camera.distance * 5.0);

            // fog hue follows the (scene-overridable) sunlight, so a scene's global
            // light tints the fog too, with a stronger fill when the sun is low
            // (midnight ≈ 2.1x) so the night doesn't go black. Overall brightness
            // tracks the sky; an extra dir-intensity pull drops night fog toward
            // the dark horizon colour (the authored night fog is darker than a
            // plain tint).
            let sun_energy = ((-next_light.sun_direction.y + 0.05) / 0.35).clamp(0.0, 1.0);
            let sun_energy = sun_energy * sun_energy * (3.0 - 2.0 * sun_energy);
            let fill = 0.8 * (1.0 + (1.0 - sun_energy) * 1.1);
            let night_pull = (next_light.sun_illuminance / 7000.0).clamp(0.35, 1.0);
            let base_color = next_light.dir_color.to_srgba() * fill * 0.5 * skybox_brightness
                / 2000.0
                * night_pull;
            let base_color = Color::from(base_color).with_alpha(1.0);

            fog.color = base_color;
            match setting.graphics.fog {
                FogSetting::Off => {
                    fog.falloff = FogFalloff::from_visibility_squared(distance * 200.0);
                    fog.directional_light_color = base_color;
                }
                FogSetting::Basic => {
                    fog.falloff = FogFalloff::from_visibility_squared(distance * 2.0);
                    fog.directional_light_color = base_color;
                }
                FogSetting::Atmospheric => {
                    fog.falloff = FogFalloff::from_visibility_squared(distance * 2.0);
                    fog.directional_light_color = next_light.dir_color;
                }
            }
            if !matches!(setting.graphics.fog, FogSetting::Off) {
                if let Some(color) = environment.fog_color {
                    fog.color = Color::linear_rgb(color.x, color.y, color.z);
                    fog.directional_light_color = fog.color;
                }
                if let Some(density) = environment.fog_density {
                    fog.falloff = FogFalloff::Exponential { density };
                }
            }
        }
    }

    // the sky envmap is the only ambient light. the pbr shaders zero the flat ambient where an
    // envmap is bound and level the envmap towards it instead: the setting's brightness, scaled
    // and tinted by a scene's ambient override, the envmap already following the time of day.
    // The compression rides in alpha, negated; a further -2 flags an authored lower sky
    // (see sky_envmap_lookup.wgsl). The uniform holds alpha * brightness. the brightness is kept above zero so the compression survives a
    // zero setting, levelling the envmap to ~nothing
    const MIN_AMBIENT_BRIGHTNESS: f32 = 0.01;
    ambient.brightness = (setting.graphics.ambient_brightness as f32
        * 20.0
        * envmap_settings.floor
        * next_light.ambient_brightness)
        .max(MIN_AMBIENT_BRIGHTNESS);
    ambient.color = next_light.ambient_color.with_alpha(
        -(envmap_settings.compression
            + if environment.nadir.is_some() {
                2.0
            } else {
                0.0
            })
            / ambient.brightness,
    );

    if prev.1.source == scene_global_light.source {
        prev.0 += time.delta_secs()
    } else {
        prev.0 = time.delta_secs()
    };
    prev.1 = base_light;
}

#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/shadows")]
struct ShadowConsoleCommand {
    on: Option<bool>,
}

fn shadow_console_command(
    mut input: ConsoleCommand<ShadowConsoleCommand>,
    mut lights: Query<&mut DirectionalLight>,
) {
    if let Some(Ok(command)) = input.take() {
        for mut light in lights.iter_mut() {
            light.shadows_enabled = command.on.unwrap_or(!light.shadows_enabled);
        }

        input.reply_ok(format!(
            "shadows {}",
            match command.on {
                None => "toggled",
                Some(true) => "enabled",
                Some(false) => "disabled",
            }
        ));
    }
}

/// The sun's shadow depth bias (a world length) and normal bias (shadow map texels): bevy's
/// defaults (see decisions.md).
pub(crate) const SUN_SHADOW_BIAS: (f32, f32) = (
    DirectionalLight::DEFAULT_SHADOW_DEPTH_BIAS,
    DirectionalLight::DEFAULT_SHADOW_NORMAL_BIAS,
);

/// The sun shadow biases set live by `/shadowbias`, in place of `SUN_SHADOW_BIAS`.
#[derive(Resource, Default)]
pub struct ShadowBiasOverride(pub Option<(f32, f32)>);

/// Set the sun shadow depth bias (world length) or normal bias (shadow map texels), keeping the
/// other at its current value; `--reset` restores the default biases.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/shadowbias")]
struct ShadowBiasConsoleCommand {
    #[arg(long)]
    depth: Option<f32>,
    #[arg(long)]
    normal: Option<f32>,
    #[arg(long, conflicts_with_all = ["depth", "normal"])]
    reset: bool,
}

/// The override after a `/shadowbias` command, or an error for a negative bias.
fn next_shadow_bias(
    current: Option<(f32, f32)>,
    preset: (f32, f32),
    depth: Option<f32>,
    normal: Option<f32>,
    reset: bool,
) -> Result<Option<(f32, f32)>, &'static str> {
    if reset {
        return Ok(None);
    }
    if depth.is_some_and(|d| d < 0.0) || normal.is_some_and(|n| n < 0.0) {
        return Err("shadow biases cannot be negative");
    }
    if depth.is_none() && normal.is_none() {
        return Ok(current);
    }
    let (d, n) = current.unwrap_or(preset);
    Ok(Some((depth.unwrap_or(d), normal.unwrap_or(n))))
}

fn shadow_bias_console_command(
    mut input: ConsoleCommand<ShadowBiasConsoleCommand>,
    mut bias: ResMut<ShadowBiasOverride>,
) {
    if let Some(Ok(command)) = input.take() {
        let preset = SUN_SHADOW_BIAS;
        match next_shadow_bias(bias.0, preset, command.depth, command.normal, command.reset) {
            Ok(next) => {
                // only write when it changes, so a query does not rewrite the lights
                if bias.0 != next {
                    bias.0 = next;
                }
                let (depth, normal) = next.unwrap_or(preset);
                input.reply_ok(format!(
                    "shadow bias depth {depth:.3} normal {normal:.3} (preset {:.3} {:.3})",
                    preset.0, preset.1
                ));
            }
            Err(error) => input.reply_failed(error),
        }
    }
}

#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/fog")]
struct FogConsoleCommand {
    on: Option<bool>,
}

fn fog_console_command(
    mut input: ConsoleCommand<FogConsoleCommand>,
    mut config: ResMut<AppConfig>,
) {
    if let Some(Ok(command)) = input.take() {
        let activate = command.on.unwrap_or(true);

        config.graphics.fog = if activate {
            FogSetting::Atmospheric
        } else {
            FogSetting::Off
        };

        input.reply_ok(format!(
            "fog {}",
            match activate {
                true => "enabled",
                false => "disabled",
            }
        ));
    }
}

#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/dof")]
struct DofConsoleCommand {
    focal_distance_extra: f32,
    sensor_height: f32,
    fstops: f32,
    max_circle: f32,
    max_depth: f32,
    mode: usize,
}

fn dof_console_command(
    mut input: ConsoleCommand<DofConsoleCommand>,
    mut cam: Query<(&mut DepthOfField, &mut DofConfig)>,
) {
    if let Some(Ok(command)) = input.take() {
        let Ok((mut dof, mut mydof)) = cam.single_mut() else {
            return;
        };

        *dof = DepthOfField {
            mode: if command.mode == 0 {
                DepthOfFieldMode::Gaussian
            } else {
                DepthOfFieldMode::Bokeh
            },
            aperture_f_stops: command.fstops,
            max_circle_of_confusion_diameter: command.max_circle,
            max_depth: command.max_depth,
            ..Default::default()
        };
        mydof.default_sensor_height = command.sensor_height;
        mydof.extra_focal_distance = command.focal_distance_extra;
        input.reply_ok("");
    }
}

fn update_dof(
    mut cam: Query<(&Transform, &DofConfig, &mut DepthOfField), With<PrimaryCamera>>,
    player: Query<&Transform, With<PrimaryUser>>,
) {
    let (Ok((cam, mydof, mut dof)), Ok(player)) = (cam.single_mut(), player.single()) else {
        return;
    };

    // let base_distance = 10.0;
    let constant_distance = mydof.extra_focal_distance;
    let current_distance = ((cam.translation - (player.translation + Vec3::Y * 1.81)).length()
        + constant_distance)
        .min(100.0);

    dof.sensor_height = mydof.default_sensor_height;
    // * ((current_distance * (current_distance + constant_distance))
    //     / (base_distance * (base_distance + constant_distance)))
    //     .sqrt();
    dof.focal_distance = current_distance;
}

/// cloud cover 0..1 (0 = clear, 1 = overcast; 0.35 is the default look) and
/// drift speed.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/cloud")]
struct CloudConsoleCommand {
    cover: f32,
    speed: Option<f32>,
}

#[derive(Resource)]
pub struct CloudCover {
    pub cover: f32,
    pub speed: f32,
    /// accumulated density that reads as fully opaque (lower = thicker clouds).
    pub density_cap: f32,
    /// shadow / minimum cloud brightness (the dark side of clouds).
    pub shadow: f32,
    /// cloud noise sample scale (higher = finer/smaller features).
    pub scale: f32,
    /// cloud ray-march step count (higher = smoother/more detail, more cost).
    pub steps: u32,
    /// per-octave frequency step of the cloud noise (default 2.345).
    pub lacunarity: f32,
    /// steepness of the logistic contrast curve on cloud coverage (0 = off).
    pub sharpness: f32,
    /// coverage value the contrast curve is centred on.
    pub sharp_mid: f32,
}

fn cloud_console_command(
    mut input: ConsoleCommand<CloudConsoleCommand>,
    mut cloud: ResMut<CloudCover>,
) {
    if let Some(Ok(command)) = input.take() {
        // the shader's cover goes from no cloud at ~0.2 to solid at ~0.7 with the
        // default sharpening; map the user's 0..1 onto that
        cloud.cover = 0.2 + command.cover * 0.5;

        if let Some(speed) = command.speed {
            cloud.speed = speed;
        }

        input.reply_ok(format!(
            "cloud cover {}, speed {}",
            command.cover, cloud.speed
        ));
    }
}

/// max accumulated density that reads as fully opaque; lower = thicker clouds.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/clouddensity")]
struct CloudDensityConsoleCommand {
    cap: f32,
}

fn cloud_density_console_command(
    mut input: ConsoleCommand<CloudDensityConsoleCommand>,
    mut cloud: ResMut<CloudCover>,
) {
    if let Some(Ok(command)) = input.take() {
        cloud.density_cap = command.cap;
        input.reply_ok(format!("cloud density cap {}", command.cap));
    }
}

/// shadow / minimum cloud brightness (the dark side of clouds).
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/cloudshadow")]
struct CloudShadowConsoleCommand {
    value: f32,
}

fn cloud_shadow_console_command(
    mut input: ConsoleCommand<CloudShadowConsoleCommand>,
    mut cloud: ResMut<CloudCover>,
) {
    if let Some(Ok(command)) = input.take() {
        cloud.shadow = command.value;
        input.reply_ok(format!("cloud shadow {}", command.value));
    }
}

/// cloud noise sample scale; higher = finer/smaller features, lower = larger.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/cloudscale")]
struct CloudScaleConsoleCommand {
    scale: f32,
}

fn cloud_scale_console_command(
    mut input: ConsoleCommand<CloudScaleConsoleCommand>,
    mut cloud: ResMut<CloudCover>,
) {
    if let Some(Ok(command)) = input.take() {
        cloud.scale = command.scale;
        input.reply_ok(format!("cloud scale {}", command.scale));
    }
}

/// cloud ray-march step count; higher = smoother/more detail, more cost.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/cloudsteps")]
struct CloudStepsConsoleCommand {
    steps: u32,
}

fn cloud_steps_console_command(
    mut input: ConsoleCommand<CloudStepsConsoleCommand>,
    mut cloud: ResMut<CloudCover>,
) {
    if let Some(Ok(command)) = input.take() {
        cloud.steps = command.steps.max(1);
        input.reply_ok(format!("cloud steps {}", cloud.steps));
    }
}

/// per-octave frequency step of the cloud noise (how much finer each successive
/// wave is); default 2.345.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/cloudlacunarity")]
struct CloudLacunarityConsoleCommand {
    lacunarity: f32,
}

fn cloud_lacunarity_console_command(
    mut input: ConsoleCommand<CloudLacunarityConsoleCommand>,
    mut cloud: ResMut<CloudCover>,
) {
    if let Some(Ok(command)) = input.take() {
        cloud.lacunarity = command.lacunarity;
        input.reply_ok(format!("cloud lacunarity {}", command.lacunarity));
    }
}

/// contrast curve on cloud coverage: logistic steepness (0 = off, default 50)
/// and the coverage it is centred on (default 0.1).
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/cloudsharp")]
struct CloudSharpConsoleCommand {
    sharpness: f32,
    mid: Option<f32>,
}

fn cloud_sharp_console_command(
    mut input: ConsoleCommand<CloudSharpConsoleCommand>,
    mut cloud: ResMut<CloudCover>,
) {
    if let Some(Ok(command)) = input.take() {
        cloud.sharpness = command.sharpness;
        if let Some(mid) = command.mid {
            cloud.sharp_mid = mid;
        }
        input.reply_ok(format!(
            "cloud sharpness {}, mid {}",
            cloud.sharpness, cloud.sharp_mid
        ));
    }
}

// --- environment grading commands ---
// expose the bevy post stack (tonemap + color grading + bloom) for live
// tuning so the environment mood can be matched by eye against a reference.

/// set the tonemapping curve.
/// options: none, reinhard, reinhard_luma, aces (default), agx, sbdt, tmmf, blender
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/tonemap")]
struct TonemapConsoleCommand {
    mode: String,
}

fn tonemap_console_command(
    mut input: ConsoleCommand<TonemapConsoleCommand>,
    mut cam: Query<&mut bevy::core_pipeline::tonemapping::Tonemapping, With<PrimaryCamera>>,
) {
    use bevy::core_pipeline::tonemapping::Tonemapping;
    if let Some(Ok(command)) = input.take() {
        let Ok(mut tonemapping) = cam.single_mut() else {
            return;
        };
        let mode = match command.mode.as_str() {
            "none" => Tonemapping::None,
            "reinhard" => Tonemapping::Reinhard,
            "reinhard_luma" => Tonemapping::ReinhardLuminance,
            "aces" => Tonemapping::AcesFitted,
            "agx" => Tonemapping::AgX,
            "sbdt" => Tonemapping::SomewhatBoringDisplayTransform,
            "tmmf" => Tonemapping::TonyMcMapface,
            "blender" => Tonemapping::BlenderFilmic,
            other => {
                input.reply_failed(format!("unknown mode `{other}`; options: none, reinhard, reinhard_luma, aces, agx, sbdt, tmmf, blender"));
                return;
            }
        };
        *tonemapping = mode;
        input.reply_ok(format!("tonemapping: {}", command.mode));
    }
}

/// set global exposure (stops, default 0.0)
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/exposure")]
struct ExposureConsoleCommand {
    exposure: f32,
}

fn exposure_console_command(
    mut input: ConsoleCommand<ExposureConsoleCommand>,
    mut cam: Query<&mut bevy::render::view::ColorGrading, With<PrimaryCamera>>,
) {
    if let Some(Ok(command)) = input.take() {
        let Ok(mut grading) = cam.single_mut() else {
            return;
        };
        grading.global.exposure = command.exposure;
        input.reply_ok(format!("exposure: {}", command.exposure));
    }
}

/// set gamma per tonal range (default 1.0 = neutral). `/gamma <shadows> [midtones] [highlights]`
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/gamma")]
struct GammaConsoleCommand {
    shadows: f32,
    midtones: Option<f32>,
    highlights: Option<f32>,
}

fn gamma_console_command(
    mut input: ConsoleCommand<GammaConsoleCommand>,
    mut cam: Query<&mut bevy::render::view::ColorGrading, With<PrimaryCamera>>,
) {
    if let Some(Ok(command)) = input.take() {
        let Ok(mut grading) = cam.single_mut() else {
            return;
        };
        let midtones = command.midtones.unwrap_or(command.shadows);
        let highlights = command.highlights.unwrap_or(midtones);
        grading.shadows.gamma = command.shadows;
        grading.midtones.gamma = midtones;
        grading.highlights.gamma = highlights;
        input.reply_ok(format!(
            "gamma: shadows {} midtones {} highlights {}",
            command.shadows, midtones, highlights
        ));
    }
}

/// set saturation per tonal range (scene default 1.3); 0 = grayscale.
/// `/saturation <shadows> [midtones] [highlights]`
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/saturation")]
struct SaturationConsoleCommand {
    shadows: f32,
    midtones: Option<f32>,
    highlights: Option<f32>,
}

fn saturation_console_command(
    mut input: ConsoleCommand<SaturationConsoleCommand>,
    mut cam: Query<&mut bevy::render::view::ColorGrading, With<PrimaryCamera>>,
) {
    if let Some(Ok(command)) = input.take() {
        let Ok(mut grading) = cam.single_mut() else {
            return;
        };
        let midtones = command.midtones.unwrap_or(command.shadows);
        let highlights = command.highlights.unwrap_or(midtones);
        grading.shadows.saturation = command.shadows;
        grading.midtones.saturation = midtones;
        grading.highlights.saturation = highlights;
        input.reply_ok(format!(
            "saturation: shadows {} midtones {} highlights {}",
            command.shadows, midtones, highlights
        ));
    }
}

/// set bloom intensity (default 0.15; 0 = off)
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/bloom")]
struct BloomConsoleCommand {
    intensity: f32,
}

fn bloom_console_command(
    mut input: ConsoleCommand<BloomConsoleCommand>,
    mut cam: Query<&mut bevy::core_pipeline::bloom::Bloom, With<PrimaryCamera>>,
) {
    if let Some(Ok(command)) = input.take() {
        let Ok(mut bloom) = cam.single_mut() else {
            return;
        };
        bloom.intensity = command.intensity;
        input.reply_ok(format!("bloom intensity: {}", command.intensity));
    }
}

#[cfg(test)]
mod tests {
    use super::next_shadow_bias;

    #[test]
    fn shadow_bias_flags_change_one_bias_at_a_time() {
        let preset = (0.02, 4.375);
        // one flag keeps the other bias, from the preset or from an earlier override
        assert_eq!(
            next_shadow_bias(None, preset, Some(0.05), None, false),
            Ok(Some((0.05, 4.375)))
        );
        assert_eq!(
            next_shadow_bias(Some((0.05, 4.375)), preset, None, Some(1.0), false),
            Ok(Some((0.05, 1.0)))
        );
        // no flag only reports; reset drops the override
        assert_eq!(
            next_shadow_bias(Some((0.05, 1.0)), preset, None, None, false),
            Ok(Some((0.05, 1.0)))
        );
        assert_eq!(
            next_shadow_bias(Some((0.05, 1.0)), preset, None, None, true),
            Ok(None)
        );
        assert!(next_shadow_bias(None, preset, Some(-0.1), None, false).is_err());
    }
}
