//! Render scale. At scale 1 the primary camera draws to the window and carries the UI, as with no
//! render scale at all. Below it, the primary camera draws the world into the scene target, an
//! image at a share of the window's pixels, and the compositor, a 2D camera over the window,
//! draws that image onto the window with FSR 1.0's EASU (`fsr_easu.wgsl`), sharpens it with
//! bevy's contrast adaptive sharpening, and carries the UI at the window's full resolution.

use bevy::{
    asset::RenderAssetUsages,
    core_pipeline::contrast_adaptive_sharpening::ContrastAdaptiveSharpening,
    image::{BevyDefault as _, ImageSampler},
    math::FloatOrd,
    prelude::*,
    render::{
        camera::{ImageRenderTarget, RenderTarget, ScalingMode},
        render_asset::RenderAssetTransferPriority,
        render_resource::{
            AsBindGroup, Extent3d, ShaderRef, TextureDimension, TextureFormat, TextureUsages,
        },
        view::RenderLayers,
    },
    sprite::{Material2d, Material2dPlugin},
    window::{PrimaryWindow, WindowRef},
};
use common::structs::{AppConfig, PrimaryCamera, SessionGraphics};

/// Contrast adaptive sharpening strength after EASU.
pub const FSR_SHARPNESS: f32 = 0.92;

/// The compositor and its quad only: keeps them out of every other camera, and keeps 3D gizmos
/// out of the compositor's 2D view.
const COMPOSITOR_LAYER: RenderLayers = RenderLayers::layer(2);

/// The image the primary camera renders the world into below scale 1 (1x1 at scale 1), and the
/// render scale in effect.
#[derive(Resource)]
pub struct SceneTarget {
    pub image: Handle<Image>,
    pub scale: f32,
}

impl FromWorld for SceneTarget {
    fn from_world(world: &mut World) -> Self {
        let mut image = Image::new_uninit(
            Extent3d::default(),
            TextureDimension::D2,
            TextureFormat::bevy_default(),
            RenderAssetUsages::all(),
        );
        image.texture_descriptor.usage =
            TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING;
        image.sampler = ImageSampler::linear();
        image.transfer_priority = RenderAssetTransferPriority::Immediate;
        Self {
            image: world.resource_mut::<Assets<Image>>().add(image),
            scale: 1.0,
        }
    }
}

/// The `Camera2d` (order 1) that draws the scene target and carries the UI below scale 1; it is
/// inactive at scale 1. While active it is the only camera drawing to the window, so it clears
/// it: while its quad's pipeline is still compiling (on the first switch below scale 1) the
/// window would otherwise show whatever was there before under the UI.
#[derive(Component)]
pub struct Compositor;

/// The compositor quad's material: EASU from the scene target.
#[derive(AsBindGroup, Asset, TypePath, Clone)]
pub struct FsrMaterial {
    #[texture(0)]
    #[sampler(1)]
    pub source: Handle<Image>,
    /// The scene target's size (xy) and the window's (zw), in physical pixels.
    #[uniform(2)]
    pub params: Vec4,
}

impl Material2d for FsrMaterial {
    fn fragment_shader() -> ShaderRef {
        "embedded://shaders/fsr_easu.wgsl".into()
    }
}

/// Physical size of the scene target for a window and scale, never below 1x1.
pub fn scene_target_size(window_physical: UVec2, render_scale: f32) -> UVec2 {
    (window_physical.as_vec2() * render_scale)
        .round()
        .as_uvec2()
        .max(UVec2::ONE)
}

/// The scene target's scale factor, which keeps its logical size equal to the window's, so the
/// camera's viewport, cursor rays and projected positions stay in window coordinates.
pub fn scene_target_scale_factor(window_scale_factor: f32, render_scale: f32) -> f32 {
    window_scale_factor * render_scale
}

pub struct UpscalePlugin;

impl Plugin for UpscalePlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(Material2dPlugin::<FsrMaterial>::default())
            .init_resource::<SceneTarget>()
            .add_systems(Startup, spawn_compositor)
            .add_systems(Update, sync_scene_target);
    }
}

fn spawn_compositor(
    mut commands: Commands,
    scene_target: Res<SceneTarget>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<FsrMaterial>>,
) {
    commands.spawn((
        Compositor,
        Camera2d,
        Camera {
            order: 1,
            is_active: false,
            ..default()
        },
        // a unit quad fills the view at any window size
        Projection::Orthographic(OrthographicProjection {
            scaling_mode: ScalingMode::Fixed {
                width: 1.0,
                height: 1.0,
            },
            ..OrthographicProjection::default_2d()
        }),
        Msaa::Off,
        ContrastAdaptiveSharpening {
            enabled: true,
            sharpening_strength: FSR_SHARPNESS,
            denoise: false,
        },
        COMPOSITOR_LAYER,
    ));
    commands.spawn((
        Mesh2d(meshes.add(Rectangle::new(1.0, 1.0))),
        MeshMaterial2d(materials.add(FsrMaterial {
            source: scene_target.image.clone(),
            params: Vec4::ONE,
        })),
        COMPOSITOR_LAYER,
    ));
}

/// Below scale 1, sizes the scene target for the window and render scale and points the primary
/// camera and the compositor's material at it. At scale 1, points the primary camera at the
/// window and shrinks the scene target to 1x1. The UI goes to whichever camera draws the window.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn sync_scene_target(
    mut commands: Commands,
    windows: Query<&Window, With<PrimaryWindow>>,
    config: Res<AppConfig>,
    session: Res<SessionGraphics>,
    mut scene_target: ResMut<SceneTarget>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<FsrMaterial>>,
    mut primary: Query<(Entity, &mut Camera), (With<PrimaryCamera>, Without<Compositor>)>,
    mut compositor: Query<(Entity, &mut Camera), With<Compositor>>,
    quads: Query<&MeshMaterial2d<FsrMaterial>>,
    mut ui_on_compositor: Local<Option<bool>>,
) {
    let (Ok(window), Ok((primary_entity, mut primary))) = (windows.single(), primary.single_mut())
    else {
        return;
    };
    let render_scale = session
        .render_scale(&config)
        .clamp(SessionGraphics::MIN_RENDER_SCALE, 1.0);
    let output = window.physical_size().max(UVec2::ONE);
    let size = scene_target_size(window.physical_size(), render_scale);
    let upscale = size != output;
    if scene_target.scale != render_scale {
        scene_target.scale = render_scale;
    }

    let image_size = if upscale { size } else { UVec2::ONE };
    let resize = images
        .get(&scene_target.image)
        .is_some_and(|image| image.size() != image_size);
    let target = if upscale {
        RenderTarget::Image(ImageRenderTarget {
            handle: scene_target.image.clone(),
            scale_factor: FloatOrd(scene_target_scale_factor(
                window.scale_factor(),
                render_scale,
            )),
        })
    } else {
        RenderTarget::Window(WindowRef::Primary)
    };
    let retarget = match (&primary.target, &target) {
        (RenderTarget::Image(current), RenderTarget::Image(wanted)) => current != wanted,
        (RenderTarget::Window(WindowRef::Primary), RenderTarget::Window(WindowRef::Primary)) => {
            false
        }
        _ => true,
    };
    if retarget {
        primary.target = target;
    }
    if retarget || resize {
        // touched either way: the image's change event is what makes bevy recompute the camera's
        // target size and scale factor
        if let Some(image) = images.get_mut(&scene_target.image) {
            image.texture_descriptor.size = Extent3d {
                width: image_size.x,
                height: image_size.y,
                depth_or_array_layers: 1,
            };
        }
    }

    // A retarget may give the image a new GPU texture, and the material's bind group only follows
    // when the material changes too, so it is touched on every retarget. Bevy prepares 2D
    // materials after the images they bind, so it binds the new texture in the same frame.
    let params = Vec4::new(
        image_size.x as f32,
        image_size.y as f32,
        output.x as f32,
        output.y as f32,
    );
    for quad in &quads {
        let stale =
            retarget || resize || materials.get(&quad.0).is_some_and(|m| m.params != params);
        if stale {
            if let Some(material) = materials.get_mut(&quad.0) {
                material.params = params;
            }
        }
    }

    let active = upscale && primary.is_active;
    for (compositor_entity, mut camera) in &mut compositor {
        if camera.is_active != active {
            camera.is_active = active;
        }
        if *ui_on_compositor != Some(upscale) {
            // the inactive compositor still targets the window, so the UI camera is named
            let (ui, other) = if upscale {
                (compositor_entity, primary_entity)
            } else {
                (primary_entity, compositor_entity)
            };
            commands.entity(ui).try_insert(IsDefaultUiCamera);
            commands.entity(other).remove::<IsDefaultUiCamera>();
            *ui_on_compositor = Some(upscale);
        }
    }
}

#[cfg(test)]
mod tests {
    use bevy::window::WindowResolution;

    use super::*;

    #[test]
    fn scene_target_size_rounds_and_clamps() {
        assert_eq!(
            scene_target_size(UVec2::new(1920, 1080), 0.5),
            UVec2::new(960, 540)
        );
        assert_eq!(
            scene_target_size(UVec2::new(1919, 1079), 0.5),
            UVec2::new(960, 540)
        );
        assert_eq!(scene_target_size(UVec2::ZERO, 0.5), UVec2::ONE);
        assert_eq!(
            scene_target_size(UVec2::new(1920, 1080), 1.0),
            UVec2::new(1920, 1080)
        );
    }

    #[test]
    fn logical_size_is_preserved() {
        assert!((scene_target_scale_factor(2.0, 0.5) - 1.0).abs() < 1e-6);
        assert!((scene_target_scale_factor(1.0, 1.0) - 1.0).abs() < 1e-6);
    }

    /// An app with the scene target system, an 800x600 primary window, the primary camera and an
    /// inactive compositor with its quad's material, as `spawn_compositor` leaves them.
    struct Rig {
        app: App,
        window: Entity,
        primary: Entity,
        compositor: Entity,
        material: Handle<FsrMaterial>,
    }

    impl Rig {
        fn new(render_scale: Option<f32>) -> Self {
            let mut app = App::new();
            app.add_plugins((MinimalPlugins, AssetPlugin::default()))
                .init_asset::<Image>()
                .init_asset::<FsrMaterial>()
                .init_resource::<SceneTarget>()
                .insert_resource(AppConfig::default())
                .insert_resource(SessionGraphics {
                    render_scale,
                    ..default()
                })
                .add_systems(Update, sync_scene_target);
            let window = app
                .world_mut()
                .spawn((
                    Window {
                        resolution: WindowResolution::new(800.0, 600.0)
                            .with_scale_factor_override(1.0),
                        ..default()
                    },
                    PrimaryWindow,
                ))
                .id();
            let primary = app
                .world_mut()
                .spawn((Camera::default(), PrimaryCamera::default()))
                .id();
            let compositor = app
                .world_mut()
                .spawn((
                    Compositor,
                    Camera {
                        order: 1,
                        is_active: false,
                        ..default()
                    },
                ))
                .id();
            let image = app.world().resource::<SceneTarget>().image.clone();
            let material = app
                .world_mut()
                .resource_mut::<Assets<FsrMaterial>>()
                .add(FsrMaterial {
                    source: image,
                    params: Vec4::ONE,
                });
            app.world_mut().spawn(MeshMaterial2d(material.clone()));
            Self {
                app,
                window,
                primary,
                compositor,
                material,
            }
        }

        fn image_size(&self) -> UVec2 {
            let image = &self.app.world().resource::<SceneTarget>().image;
            self.app
                .world()
                .resource::<Assets<Image>>()
                .get(image)
                .unwrap()
                .size()
        }

        fn camera(&self, entity: Entity) -> &Camera {
            self.app.world().get::<Camera>(entity).unwrap()
        }

        fn ui_camera(&self) -> Option<Entity> {
            let ui = |entity| self.app.world().get::<IsDefaultUiCamera>(entity).is_some();
            match (ui(self.primary), ui(self.compositor)) {
                (true, false) => Some(self.primary),
                (false, true) => Some(self.compositor),
                _ => None,
            }
        }

        fn resize(&mut self, width: u32, height: u32) {
            self.app
                .world_mut()
                .get_mut::<Window>(self.window)
                .unwrap()
                .resolution
                .set_physical_resolution(width, height);
        }

        /// Runs a frame and reports whether the compositor's material changed in it.
        fn frame(&mut self) -> bool {
            self.app.update();
            let id = self.material.id();
            self.app
                .world_mut()
                .resource_mut::<Events<AssetEvent<FsrMaterial>>>()
                .drain()
                .any(|event| matches!(event, AssetEvent::Modified { id: modified } if modified == id))
        }
    }

    /// At scale 1 nothing is added: the primary camera draws to the window and carries the UI,
    /// the compositor stays inactive and the scene target holds a single pixel.
    #[test]
    fn scale_one_draws_straight_to_the_window() {
        let mut rig = Rig::new(Some(1.0));
        rig.frame();
        assert!(matches!(
            rig.camera(rig.primary).target,
            RenderTarget::Window(WindowRef::Primary)
        ));
        assert!(!rig.camera(rig.compositor).is_active);
        assert_eq!(rig.ui_camera(), Some(rig.primary));
        assert_eq!(rig.image_size(), UVec2::ONE);
    }

    /// An 800x600 window at half scale draws the world at 400x300 into the scene target, which
    /// the active compositor draws with the UI; back at scale 1 the primary camera takes the
    /// window again, and a window minimised to nothing has nothing to upscale.
    #[test]
    fn scene_target_follows_the_scale_and_the_window() {
        let mut rig = Rig::new(Some(0.5));
        rig.frame();
        assert_eq!(rig.image_size(), UVec2::new(400, 300));
        match &rig.camera(rig.primary).target {
            RenderTarget::Image(target) => {
                assert_eq!(
                    target.handle,
                    rig.app.world().resource::<SceneTarget>().image
                );
                assert_eq!(target.scale_factor.0, 0.5);
            }
            other => panic!("primary camera renders to {other:?}"),
        }
        assert!(rig.camera(rig.compositor).is_active);
        assert_eq!(rig.ui_camera(), Some(rig.compositor));
        assert_eq!(rig.app.world().resource::<SceneTarget>().scale, 0.5);

        rig.app
            .world_mut()
            .resource_mut::<SessionGraphics>()
            .render_scale = Some(1.0);
        rig.frame();
        assert!(matches!(
            rig.camera(rig.primary).target,
            RenderTarget::Window(WindowRef::Primary)
        ));
        assert!(!rig.camera(rig.compositor).is_active);
        assert_eq!(rig.ui_camera(), Some(rig.primary));
        assert_eq!(rig.image_size(), UVec2::ONE);

        rig.app
            .world_mut()
            .resource_mut::<SessionGraphics>()
            .render_scale = Some(0.5);
        rig.resize(0, 0);
        rig.frame();
        assert!(!rig.camera(rig.compositor).is_active);
        assert_eq!(rig.image_size(), UVec2::ONE);
    }

    /// The compositor's material is touched in the frame the scene target changes and left alone
    /// after it.
    #[test]
    fn material_follows_the_scene_target() {
        let mut rig = Rig::new(Some(0.5));
        assert_eq!([rig.frame(), rig.frame()], [true, false]);
        rig.resize(1024, 768);
        assert_eq!([rig.frame(), rig.frame()], [true, false]);
        let material = rig
            .app
            .world()
            .resource::<Assets<FsrMaterial>>()
            .get(&rig.material)
            .unwrap();
        assert_eq!(material.params, Vec4::new(512.0, 384.0, 1024.0, 768.0));
    }

    /// The compositor's pipeline compiles on this machine's adapter, in a renderer with no
    /// window, and draws: one 2D camera upscales a 64x64 source onto a 128x128 image with EASU
    /// and the sharpening after it. Needs a GPU, so `#[ignore]`d and skipped by CI:
    ///
    /// ```text
    /// cargo test -p visuals fsr_material_compiles -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs a GPU; run with --ignored"]
    fn fsr_material_compiles() {
        use std::time::{Duration, Instant};

        use bevy::{
            log::LogPlugin,
            render::view::screenshot::{Screenshot, ScreenshotCaptured},
            render::{
                pipelined_rendering::PipelinedRenderingPlugin,
                render_resource::{CachedPipelineState, PipelineCache, PipelineDescriptor},
                RenderApp,
            },
            window::ExitCondition,
            winit::WinitPlugin,
        };

        let mut app = App::new();
        app.add_plugins(
            DefaultPlugins
                .build()
                .disable::<LogPlugin>()
                .disable::<WinitPlugin>()
                // keeps the render app in this app, so the pipeline cache is readable
                .disable::<PipelinedRenderingPlugin>()
                .set(WindowPlugin {
                    primary_window: None,
                    exit_condition: ExitCondition::DontExit,
                    close_when_requested: false,
                }),
        );
        app.add_plugins((assets::EmbedAssetsPlugin, UpscalePlugin))
            .insert_resource(AppConfig::default())
            .init_resource::<SessionGraphics>();
        app.finish();
        app.cleanup();

        let world = app.world_mut();
        let extent = |size: u32| Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: 1,
        };
        let pattern: Vec<u8> = (0..64u8)
            .flat_map(|y| (0..64u8).flat_map(move |x| [x, y, x ^ y, 255]))
            .collect();
        let mut images = world.resource_mut::<Assets<Image>>();
        let source = images.add(Image::new(
            extent(64),
            TextureDimension::D2,
            pattern,
            TextureFormat::bevy_default(),
            RenderAssetUsages::all(),
        ));
        // a blank output, so a compositor that never draws fails the check
        let mut output = Image::new_fill(
            extent(128),
            TextureDimension::D2,
            &[0, 0, 0, 0],
            TextureFormat::bevy_default(),
            RenderAssetUsages::all(),
        );
        output.texture_descriptor.usage |= TextureUsages::RENDER_ATTACHMENT;
        let output = images.add(output);
        let mesh = world
            .resource_mut::<Assets<Mesh>>()
            .add(Rectangle::new(1.0, 1.0));
        let material = world
            .resource_mut::<Assets<FsrMaterial>>()
            .add(FsrMaterial {
                source,
                params: Vec4::new(64.0, 64.0, 128.0, 128.0),
            });
        let layer = RenderLayers::layer(7);
        world.spawn((
            Camera2d,
            Camera {
                target: RenderTarget::Image(output.clone().into()),
                ..default()
            },
            Projection::Orthographic(OrthographicProjection {
                scaling_mode: ScalingMode::Fixed {
                    width: 1.0,
                    height: 1.0,
                },
                ..OrthographicProjection::default_2d()
            }),
            Msaa::Off,
            ContrastAdaptiveSharpening {
                enabled: true,
                sharpening_strength: FSR_SHARPNESS,
                denoise: false,
            },
            layer.clone(),
        ));
        world.spawn((Mesh2d(mesh), MeshMaterial2d(material), layer));

        // a few frames queue every pipeline; then they compile in the background, and once they
        // are ready a few more frames draw with them
        for _ in 0..10 {
            app.update();
        }
        let started = Instant::now();
        let mut ok;
        let mut errors = Vec::new();
        loop {
            app.update();
            let cache = app.sub_app(RenderApp).world().resource::<PipelineCache>();
            ok = 0;
            errors.clear();
            let mut pending = 0;
            for pipeline in cache.pipelines() {
                match &pipeline.state {
                    CachedPipelineState::Ok(_) => ok += 1,
                    CachedPipelineState::Err(error) => errors.push(error.to_string()),
                    _ => pending += 1,
                }
            }
            if pending == 0 {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "{pending} pipelines still compiling"
            );
        }
        let drawn = std::sync::Arc::new(std::sync::Mutex::new(None));
        let sink = drawn.clone();
        app.world_mut().spawn(Screenshot::image(output)).observe(
            move |trigger: Trigger<ScreenshotCaptured>| {
                *sink.lock().unwrap() = Some(trigger.0.clone());
            },
        );
        for _ in 0..5 {
            app.update();
        }
        println!("{ok} pipelines compiled");
        assert!(
            errors.is_empty(),
            "pipeline errors:\n{}",
            errors.join("\n\n")
        );

        let shader = app
            .world()
            .resource::<AssetServer>()
            .load::<Shader>("embedded://shaders/fsr_easu.wgsl")
            .id();
        let compositor_pipelines = app
            .sub_app(RenderApp)
            .world()
            .resource::<PipelineCache>()
            .pipelines()
            .filter(|pipeline| match &pipeline.descriptor {
                PipelineDescriptor::RenderPipelineDescriptor(descriptor) => descriptor
                    .fragment
                    .as_ref()
                    .is_some_and(|fragment| fragment.shader.id() == shader),
                _ => false,
            })
            .count();
        assert_eq!(compositor_pipelines, 1);

        let drawn = drawn
            .lock()
            .unwrap()
            .take()
            .expect("the output was captured");
        assert!(
            drawn
                .data
                .as_deref()
                .is_some_and(|data| data.iter().any(|&byte| byte != 0)),
            "the compositor drew nothing"
        );
    }
}
