use bevy::{
    core_pipeline::prepass::{DeferredPrepass, DepthPrepass},
    pbr::{CascadeShadowConfig, DirectionalLightShadowMap, RenderMeshInstances},
    prelude::*,
    render::{
        batching::gpu_preprocessing::GpuPreprocessingSupport,
        experimental::occlusion_culling::OcclusionCulling, view::NoIndirectDrawing, RenderApp,
    },
};
use bevy_console::ConsoleCommand;
use common::structs::{AppConfig, PrimaryCamera, SceneLoadDistance, TimeOfDay};
use console::{AddDebugSection, DebugInput, DebugInputKind, DebugSection, DoAddConsoleCommand};
use serde_json::json;

use crate::{
    shell_texturing::{Ground, ParcelGrass},
    trees::Tree,
    CloudCover, ShadowBiasOverride, SUN_SHADOW_BIAS,
};

const fn toggle(
    label: &'static str,
    field: &'static str,
    command: &'static str,
) -> DebugInput<'static> {
    DebugInput {
        label,
        field,
        command,
        kind: DebugInputKind::Toggle,
    }
}

const fn slider(
    label: &'static str,
    field: &'static str,
    command: &'static str,
    min: f64,
    max: f64,
    step: f64,
) -> DebugInput<'static> {
    DebugInput {
        label,
        field,
        command,
        kind: DebugInputKind::Number { min, max, step },
    }
}

pub(super) struct VisualsDebugPanelPlugin;

impl Plugin for VisualsDebugPanelPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(LandscapeLayers {
            ground: true,
            trees: true,
            grass: true,
        })
        .add_console_command::<LandscapeCommand, _>(landscape_command)
        .add_console_command::<SkyState, _>(sky_state)
        .add_console_command::<RenderView, _>(render_view)
        .add_console_command::<SunShadows, _>(sun_shadows)
        .add_console_command::<SunShadowMap, _>(sun_shadow_map)
        .add_preview_debug_inputs(DebugSection {
            title: "Landscape",
            query: "/landscape",
            fields: &[
                ("Scene load distance (m)", "loadDistance"),
                ("Loaded trees", "treeCount"),
                ("Grass parcels", "grassParcels"),
            ],
            actions: &[],
            inputs: &[
                toggle("Ground", "ground", "/landscape ground {value}"),
                toggle("Trees", "trees", "/landscape trees {value}"),
                toggle("Parcel grass", "grass", "/landscape grass {value}"),
            ],
        })
        .add_debug_section(DebugSection {
            title: "Sun shadows",
            query: "/sun_shadows",
            fields: &[
                ("Shadow quality", "quality"),
                ("Shadow distance (m)", "distance"),
                ("Cascade far bounds (m)", "cascades"),
                ("Bias overridden", "overridden"),
                ("Default bias (depth m / normal texels)", "presetBias"),
            ],
            actions: &[("Restore default bias", "/shadowbias --reset")],
            inputs: &[
                DebugInput {
                    label: "Shadow map size (texels)",
                    field: "mapSize",
                    command: "/shadow_map {value}",
                    kind: DebugInputKind::Select(&[
                        ("512", "512"),
                        ("1024", "1024"),
                        ("2048", "2048"),
                        ("4096 (default)", "4096"),
                    ]),
                },
                slider(
                    "Depth bias (m)",
                    "depthBias",
                    "/shadowbias --depth {value}",
                    0.0,
                    1.0,
                    0.005,
                ),
                slider(
                    "Normal bias (texels)",
                    "normalBias",
                    "/shadowbias --normal {value}",
                    0.0,
                    10.0,
                    0.05,
                ),
            ],
        })
        .add_debug_section(DebugSection {
            title: "Skybox",
            query: "/sky_state",
            fields: &[],
            actions: &[("Resume sky clock", "/time --resume")],
            inputs: &[
                slider("Fixed hour", "hour", "/time {value} 0", 0.0, 23.99, 0.01),
                slider("Cloud cover", "cover", "/cloud {value}", 0.0, 1.0, 0.01),
                slider(
                    "Cloud density",
                    "density",
                    "/clouddensity {value}",
                    0.01,
                    3.0,
                    0.01,
                ),
                slider(
                    "Cloud shadows",
                    "shadow",
                    "/cloudshadow {value}",
                    0.0,
                    1.0,
                    0.01,
                ),
                slider(
                    "Cloud scale",
                    "scale",
                    "/cloudscale {value}",
                    0.01,
                    10.0,
                    0.01,
                ),
                slider(
                    "Cloud march steps",
                    "steps",
                    "/cloudsteps {value}",
                    1.0,
                    128.0,
                    1.0,
                ),
                slider(
                    "Cloud octave frequency",
                    "lacunarity",
                    "/cloudlacunarity {value}",
                    1.0,
                    5.0,
                    0.01,
                ),
                slider(
                    "Cloud edge sharpness",
                    "sharpness",
                    "/cloudsharp {value}",
                    0.0,
                    100.0,
                    1.0,
                ),
            ],
        });
        if app
            .world()
            .get_resource::<common::structs::PreviewMode>()
            .is_some_and(|p| p.is_preview)
        {
            app.add_systems(Update, apply_landscape_visibility);
        }
    }

    fn finish(&self, app: &mut App) {
        let render = app.sub_app(RenderApp).world();
        let gpu_instances = matches!(
            render.get_resource::<RenderMeshInstances>(),
            Some(RenderMeshInstances::GpuBuilding(_))
        );
        let culling = gpu_instances
            && render
                .get_resource::<GpuPreprocessingSupport>()
                .is_some_and(|s| s.is_culling_supported());
        app.insert_resource(RenderCapabilities {
            gpu_instances,
            culling,
        });
        let inputs = [
            toggle(
                "Indirect drawing",
                "indirect",
                "/render_view indirect {value}",
            ),
            toggle(
                "GPU occlusion culling",
                "occlusion",
                "/render_view occlusion {value}",
            ),
        ];
        app.add_preview_debug_inputs(DebugSection {
            title: "GPU rendering",
            query: "/render_view",
            fields: &[
                ("GPU-built mesh instances", "gpuInstances"),
                ("GPU culling supported", "supported"),
            ],
            actions: &[],
            inputs: if culling { &inputs } else { &[] },
        });
    }
}

#[derive(Resource)]
struct LandscapeLayers {
    ground: bool,
    trees: bool,
    grass: bool,
}

/// Change visibility for a landscape render layer, including subsequently streamed entities.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/landscape")]
struct LandscapeCommand {
    #[arg(value_parser = ["ground", "trees", "grass"], requires = "visible")]
    layer: Option<String>,
    #[arg(requires = "layer")]
    visible: Option<bool>,
}

fn landscape_command(
    mut cmd: ConsoleCommand<LandscapeCommand>,
    preview: Option<Res<common::structs::PreviewMode>>,
    mut layers: ResMut<LandscapeLayers>,
    load: Res<SceneLoadDistance>,
    trees: Query<(), With<Tree>>,
    grass: Query<(), With<ParcelGrass>>,
) {
    let Some(Ok(args)) = cmd.take() else { return };
    if let (Some(layer), Some(value)) = (args.layer.as_deref(), args.visible) {
        if !preview.is_some_and(|p| p.is_preview) {
            cmd.reply_failed("landscape overrides require preview mode");
            return;
        }
        match layer {
            "ground" => layers.ground = value,
            "trees" => layers.trees = value,
            _ => layers.grass = value,
        }
    }
    cmd.reply_ok(json!({"ground":layers.ground, "trees":layers.trees, "grass":layers.grass,
        "treeCount":trees.iter().len(), "grassParcels":grass.iter().len(), "loadDistance":load.load}).to_string());
}

type LandscapeObjects = Or<(With<Ground>, With<Tree>, With<ParcelGrass>)>;
type AddedLandscapeObjects = Or<(Added<Ground>, Added<Tree>, Added<ParcelGrass>)>;

fn apply_landscape_visibility(
    layers: Res<LandscapeLayers>,
    mut objects: Query<(Has<Ground>, Has<Tree>, &mut Visibility), LandscapeObjects>,
    added: Query<Entity, AddedLandscapeObjects>,
) {
    let set = |(ground, tree, mut visibility): (bool, bool, Mut<Visibility>)| {
        let show = if ground {
            layers.ground
        } else if tree {
            layers.trees
        } else {
            layers.grass
        };
        visibility.set_if_neq(if show {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        });
    };
    if layers.is_changed() {
        objects.iter_mut().for_each(set);
    } else if !layers.ground || !layers.trees || !layers.grass {
        for entity in &added {
            if let Ok(object) = objects.get_mut(entity) {
                set(object);
            }
        }
    }
}

/// Read the effective rendered sky time and atmosphere settings.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/sky_state")]
struct SkyState;

fn sky_state(mut cmd: ConsoleCommand<SkyState>, time: Res<TimeOfDay>, clouds: Res<CloudCover>) {
    if let Some(Ok(_)) = cmd.take() {
        cmd.reply_ok(json!({"hour":time.time / 3600., "cover":(clouds.cover - 0.2) / 0.5, "density":clouds.density_cap,
            "shadow":clouds.shadow, "scale":clouds.scale, "steps":clouds.steps, "lacunarity":clouds.lacunarity, "sharpness":clouds.sharpness}).to_string());
    }
}

/// Read the sun's effective shadow quality, cascades and biases.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/sun_shadows")]
struct SunShadows;

fn sun_shadows(
    mut cmd: ConsoleCommand<SunShadows>,
    config: Res<AppConfig>,
    bias: Res<ShadowBiasOverride>,
    map: Res<DirectionalLightShadowMap>,
    sun: Query<(&DirectionalLight, &CascadeShadowConfig)>,
) {
    if let Some(Ok(_)) = cmd.take() {
        let preset = SUN_SHADOW_BIAS;
        let (depth, normal) = bias.0.unwrap_or(preset);
        // every sun layer shares one config; read the first that casts shadows
        let cascades = sun
            .iter()
            .find(|(light, _)| light.shadows_enabled)
            .map(|(_, cascades)| cascades.bounds.clone());
        cmd.reply_ok(
            json!({"quality":format!("{:?}", config.graphics.shadow_settings),
            "distance":config.graphics.shadow_distance, "mapSize":map.size.to_string(),
            "cascades":cascades, "overridden":bias.0.is_some(), "presetBias":[preset.0, preset.1],
            "depthBias":depth, "normalBias":normal})
            .to_string(),
        );
    }
}

/// Set the sun's shadow map size in texels per cascade until restart, to compare biases at
/// another resolution. The normal bias is in texels, so the same value offsets further on a
/// smaller map.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/shadow_map")]
struct SunShadowMap {
    size: usize,
}

fn sun_shadow_map(
    mut cmd: ConsoleCommand<SunShadowMap>,
    mut map: ResMut<DirectionalLightShadowMap>,
) {
    if let Some(Ok(SunShadowMap { size })) = cmd.take() {
        if !(256..=8192).contains(&size) || !size.is_power_of_two() {
            cmd.reply_failed("shadow map size must be a power of two from 256 to 8192");
            return;
        }
        map.size = size;
        cmd.reply_ok(format!("sun shadow map {size} texels"));
    }
}

#[derive(Resource)]
struct RenderCapabilities {
    gpu_instances: bool,
    culling: bool,
}

/// Inspect main camera GPU drawing or set its indirect draw and occlusion mode.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/render_view")]
struct RenderView {
    #[arg(value_parser = ["indirect", "occlusion"], requires = "enabled")]
    feature: Option<String>,
    #[arg(requires = "feature")]
    enabled: Option<bool>,
}

#[allow(clippy::type_complexity)]
fn render_view(
    mut cmd: ConsoleCommand<RenderView>,
    preview: Option<Res<common::structs::PreviewMode>>,
    mut commands: Commands,
    caps: Res<RenderCapabilities>,
    camera: Query<
        (
            Entity,
            Has<NoIndirectDrawing>,
            Has<OcclusionCulling>,
            Has<DepthPrepass>,
            Has<DeferredPrepass>,
        ),
        With<PrimaryCamera>,
    >,
) {
    let Some(Ok(args)) = cmd.take() else { return };
    let Ok((entity, direct, occlusion, depth, deferred)) = camera.single() else {
        cmd.reply_failed("player camera unavailable");
        return;
    };
    let mut indirect = caps.culling && !direct;
    let mut occlusion = indirect && depth && !deferred && occlusion;
    if let (Some(feature), Some(enabled)) = (args.feature.as_deref(), args.enabled) {
        if !preview.is_some_and(|p| p.is_preview) {
            cmd.reply_failed("renderer overrides require preview mode");
            return;
        }
        if !caps.culling {
            cmd.reply_failed("GPU indirect culling is unavailable");
            return;
        }
        let mut view = commands.entity(entity);
        match feature {
            "indirect" => {
                indirect = enabled;
                if enabled {
                    view.remove::<NoIndirectDrawing>();
                } else {
                    view.insert(NoIndirectDrawing).remove::<OcclusionCulling>();
                    occlusion = false;
                }
            }
            _ => {
                if enabled && !(indirect && depth && !deferred) {
                    cmd.reply_failed(
                        "occlusion needs indirect drawing and a forward depth prepass",
                    );
                    return;
                }
                occlusion = enabled;
                if enabled {
                    view.insert(OcclusionCulling);
                } else {
                    view.remove::<OcclusionCulling>();
                }
            }
        }
    }
    cmd.reply_ok(json!({"gpuInstances":caps.gpu_instances, "supported":caps.culling, "indirect":indirect, "occlusion":occlusion}).to_string());
}
