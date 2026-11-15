//! The setup the `#[ignore]`d pipeline-compile tests share (see recipes.md). Only ever a
//! dev-dependency.

use std::time::{Duration, Instant};

use bevy::{
    asset::RenderAssetUsages,
    log::LogPlugin,
    prelude::*,
    render::{
        pipelined_rendering::PipelinedRenderingPlugin,
        render_resource::{
            CachedPipelineState, Extent3d, PipelineCache, PipelineDescriptor, TextureDimension,
            TextureFormat, TextureUsages,
        },
        RenderApp,
    },
    window::ExitCondition,
    winit::WinitPlugin,
};

use assets::EmbedAssetsPlugin;

/// An app with the default plugins, a renderer with no window and the embedded assets. Add the
/// plugins under test, then `finish` and `cleanup` it.
pub fn headless_app() -> App {
    let mut app = headless_app_without_embedded_assets();
    app.add_plugins(EmbedAssetsPlugin);
    app
}

/// Register `EmbedAssetsPlugin` after the plugins under test to exercise the client's asset
/// registration order. Shader dependency requests must wait until startup.
pub fn headless_app_without_embedded_assets() -> App {
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
    app
}

/// A small image for a camera to render to.
pub fn render_target(app: &mut App) -> Handle<Image> {
    let mut target = Image::new_fill(
        Extent3d {
            width: 64,
            height: 64,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        &[0, 0, 0, 0],
        TextureFormat::Bgra8UnormSrgb,
        RenderAssetUsages::all(),
    );
    target.texture_descriptor.usage |= TextureUsages::RENDER_ATTACHMENT;
    app.world_mut().resource_mut::<Assets<Image>>().add(target)
}

/// Updates the app until every pipeline it queues has compiled, and returns how many did. Panics
/// on a pipeline error, or when one is still compiling after two minutes.
pub fn compile_pipelines(app: &mut App) -> usize {
    // a few frames queue every pipeline; then they compile in the background
    for _ in 0..10 {
        app.update();
    }
    let started = Instant::now();
    loop {
        app.update();
        let cache = app.sub_app(RenderApp).world().resource::<PipelineCache>();
        let (mut ok, mut pending, mut errors) = (0, 0, Vec::new());
        for pipeline in cache.pipelines() {
            match &pipeline.state {
                CachedPipelineState::Ok(_) => ok += 1,
                CachedPipelineState::Err(error) => errors.push(error.to_string()),
                _ => pending += 1,
            }
        }
        if pending == 0 {
            assert!(
                errors.is_empty(),
                "pipeline errors:\n{}",
                errors.join("\n\n")
            );
            println!("{ok} pipelines compiled");
            return ok;
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "{pending} pipelines still compiling"
        );
    }
}

/// How many compiled pipelines have the shader at `path` as their fragment stage. Panics when
/// that shader never loaded.
pub fn fragment_pipelines(app: &App, path: &str) -> usize {
    let shader = app
        .world()
        .resource::<AssetServer>()
        .get_handle::<Shader>(path)
        .unwrap_or_else(|| panic!("{path} never loaded"))
        .id();
    let cache = app.sub_app(RenderApp).world().resource::<PipelineCache>();
    cache
        .pipelines()
        .filter(|pipeline| {
            matches!(
                &pipeline.descriptor,
                PipelineDescriptor::RenderPipelineDescriptor(descriptor)
                    if descriptor.fragment.as_ref()
                        .is_some_and(|fragment| fragment.shader.id() == shader)
            ) && matches!(pipeline.state, CachedPipelineState::Ok(_))
        })
        .count()
}
