//! The hover glow: highlighted meshes drawn as widened silhouettes into a mask, blurred, cut out
//! where the mesh itself covers the mask, and blended over the frame after depth of field, through
//! anything in front of them.

use bevy::{
    app::HierarchyPropagatePlugin,
    core_pipeline::core_3d::graph::{Core3d, Node3d},
    prelude::*,
    render::{
        extract_component::{ExtractComponent, ExtractComponentPlugin},
        render_graph::{RenderGraphApp, ViewNodeRunner},
        render_phase::{
            sort_phase_system, AddRenderCommand, DrawFunctions, SortedRenderPhasePlugin,
        },
        render_resource::SpecializedMeshPipelines,
        ExtractSchedule, Render, RenderApp, RenderDebugFlags, RenderSet,
    },
};

mod node;
mod phase;

pub use node::HighlightLabel;
use node::{prepare_highlight_textures, HighlightNode, HighlightPostPipelines};
use phase::{
    extract_highlight_phases, prepare_highlight_bind_group, queue_highlight_meshes,
    DrawHighlightMask, DrawHighlightPunch, HighlightBatch, HighlightItemUniforms,
    HighlightMaskItem, HighlightMaskPipeline,
};

/// Put on a Mesh3d entity (propagate from the scene entity with `Propagate(Highlight)`).
#[derive(Component, Clone, Copy, PartialEq, Debug, ExtractComponent)]
pub struct Highlight {
    pub color: LinearRgba,
    /// the silhouette's widening, as a share of the view's height
    pub width: f32,
}

pub const HIGHLIGHT_VALID: Color = Color::srgb(0.23843002, 0.8867924, 0.45072904);
pub const HIGHLIGHT_INVALID: Color = Color::srgb(1.0, 0.17647058, 0.49867108);
/// A share of the view's height: 4 pixels at 1080p, 8 at 2160p, the same on screen at every
/// window size, pixel ratio and render scale.
pub const HIGHLIGHT_WIDTH: f32 = 4.0 / 1080.0;
#[allow(clippy::excessive_precision)]
pub const BLUR_OFFSETS: [f32; 3] = [0.0, 1.3846153846, 3.2307692308];
#[allow(clippy::excessive_precision)]
pub const BLUR_WEIGHTS: [f32; 3] = [0.2270270270, 0.3162162162, 0.0702702703];
/// The glow target's height in pixels, its width following the view's aspect. The blur's reach is
/// counted in its texels, so it spreads the same share of the view, at the same cost, at every
/// resolution; at 1080p it is half the view, where one blur spreads as far as four at full size.
pub const GLOW_HEIGHT: u32 = 540;

pub struct HighlightPlugin;

impl Plugin for HighlightPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(HierarchyPropagatePlugin::<Highlight>::default());
        // headless apps have no renderer, and nothing to sync the component to
        if app.get_sub_app(RenderApp).is_none() {
            return;
        }
        app.add_plugins((
            ExtractComponentPlugin::<Highlight>::extract_visible(),
            SortedRenderPhasePlugin::<HighlightMaskItem, HighlightBatch>::new(
                RenderDebugFlags::default(),
            ),
        ));

        app.sub_app_mut(RenderApp)
            .init_resource::<SpecializedMeshPipelines<HighlightMaskPipeline>>()
            .init_resource::<DrawFunctions<HighlightMaskItem>>()
            .init_resource::<HighlightItemUniforms>()
            .add_render_command::<HighlightMaskItem, DrawHighlightMask>()
            .add_render_command::<HighlightMaskItem, DrawHighlightPunch>()
            .add_systems(ExtractSchedule, extract_highlight_phases)
            .add_systems(
                Render,
                (
                    queue_highlight_meshes.in_set(RenderSet::QueueMeshes),
                    sort_phase_system::<HighlightMaskItem>.in_set(RenderSet::PhaseSort),
                    prepare_highlight_textures.in_set(RenderSet::PrepareResources),
                    prepare_highlight_bind_group.in_set(RenderSet::PrepareBindGroups),
                ),
            )
            .add_render_graph_node::<ViewNodeRunner<HighlightNode>>(Core3d, HighlightLabel)
            .add_render_graph_edges(
                Core3d,
                (Node3d::DepthOfField, HighlightLabel, Node3d::Tonemapping),
            );
    }

    fn finish(&self, app: &mut App) {
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app
                .init_resource::<HighlightMaskPipeline>()
                .init_resource::<HighlightPostPipelines>();
        }
    }
}

#[cfg(test)]
mod test {
    use bevy::{
        asset::{RenderAssetTransferPriority, RenderAssetUsages},
        core_pipeline::prepass::{DepthPrepass, NormalPrepass},
        render::{
            camera::RenderTarget,
            mesh::{
                morph::{MeshMorphWeights, MorphAttributes, MorphTargetImage},
                skinning::{SkinnedMesh, SkinnedMeshInverseBindposes},
                VertexAttributeValues,
            },
            render_resource::{
                PipelineCache, PipelineDescriptor, RenderPipelineDescriptor, ShaderDefVal,
            },
            storage::ShaderStorageBuffer,
        },
    };
    use pipeline_test::{compile_pipelines, headless_app, render_target};

    use super::*;
    use crate::{SceneBound, SceneMaterial, EMPTY_SCENE_CELLS};

    /// The highlight pipelines compile on this machine's adapter, and the node draws with them, in
    /// a renderer with no window: a plain cube in the scene material, a skinned cube and a
    /// morph-target cube, each highlighted, in front of a camera with the depth and normal
    /// prepasses the app's cameras carry. Needs a GPU, so `#[ignore]`d and skipped by CI:
    ///
    /// ```text
    /// cargo test -p scene_material highlight_pipelines_compile -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs a GPU; run with --ignored"]
    fn highlight_pipelines_compile_with_prepasses() {
        highlight_pipelines_compile(true);
    }

    /// As above, for a camera with no prepass: the mask pipelines follow the view's bind group
    /// layout either way.
    #[test]
    #[ignore = "needs a GPU; run with --ignored"]
    fn highlight_pipelines_compile_without_prepasses() {
        highlight_pipelines_compile(false);
    }

    fn highlight_pipelines_compile(prepasses: bool) {
        let mut app = headless_app();
        app.add_plugins((MaterialPlugin::<SceneMaterial>::default(), HighlightPlugin));
        app.finish();
        app.cleanup();

        let target = render_target(&mut app);
        let world = app.world_mut();
        // the unbounded scene's cell table, as SceneBoundPlugin inserts it
        world.resource_mut::<Assets<ShaderStorageBuffer>>().insert(
            EMPTY_SCENE_CELLS.id(),
            ShaderStorageBuffer::new(&[0; 8], RenderAssetUsages::RENDER_WORLD),
        );

        let cube = Mesh::from(Cuboid::new(0.5, 0.5, 0.5));
        let vertex_count = cube.count_vertices();
        let mut skinned = cube.clone();
        skinned.insert_attribute(
            Mesh::ATTRIBUTE_JOINT_INDEX,
            VertexAttributeValues::Uint16x4(vec![[0, 1, 0, 0]; vertex_count]),
        );
        skinned.insert_attribute(
            Mesh::ATTRIBUTE_JOINT_WEIGHT,
            vec![[0.5f32, 0.5, 0.0, 0.0]; vertex_count],
        );
        let morph_targets = MorphTargetImage::new(
            [(0..vertex_count)
                .map(|_| MorphAttributes::new(Vec3::Y * 0.1, Vec3::ZERO, Vec3::ZERO))]
            .into_iter(),
            vertex_count,
            RenderAssetUsages::RENDER_WORLD,
            RenderAssetTransferPriority::default(),
        )
        .unwrap();
        let morph_targets = world.resource_mut::<Assets<Image>>().add(morph_targets.0);
        let mut morphed = cube.clone();
        morphed.set_morph_targets(morph_targets);

        let mut meshes = world.resource_mut::<Assets<Mesh>>();
        let (cube, skinned, morphed) = (meshes.add(cube), meshes.add(skinned), meshes.add(morphed));
        let scene = world
            .resource_mut::<Assets<SceneMaterial>>()
            .add(SceneMaterial {
                base: StandardMaterial::default(),
                extension: SceneBound::new_unbounded(),
            });
        let standard = world
            .resource_mut::<Assets<StandardMaterial>>()
            .add(StandardMaterial::default());
        let inverse_bindposes = world
            .resource_mut::<Assets<SkinnedMeshInverseBindposes>>()
            .add(SkinnedMeshInverseBindposes::from(vec![Mat4::IDENTITY; 2]));
        let highlight = Highlight {
            color: HIGHLIGHT_VALID.into(),
            width: HIGHLIGHT_WIDTH,
        };

        // the player's camera: only it draws the glow
        let camera = world
            .spawn((
                Camera3d::default(),
                common::structs::PrimaryCamera::default(),
                Camera {
                    target: RenderTarget::Image(target.into()),
                    ..default()
                },
                Transform::from_xyz(0.0, 0.0, 4.0).looking_at(Vec3::ZERO, Vec3::Y),
            ))
            .id();
        if prepasses {
            world
                .entity_mut(camera)
                .insert((DepthPrepass, NormalPrepass));
        }
        world.spawn((
            Mesh3d(cube),
            MeshMaterial3d(scene),
            Transform::from_xyz(-1.0, 0.0, 0.0),
            highlight,
        ));
        let joints = vec![
            world.spawn(Transform::default()).id(),
            world.spawn(Transform::from_xyz(0.0, 0.2, 0.0)).id(),
        ];
        world.spawn((
            Mesh3d(skinned),
            MeshMaterial3d(standard.clone()),
            SkinnedMesh {
                inverse_bindposes,
                joints,
            },
            highlight,
        ));
        world.spawn((
            Mesh3d(morphed),
            MeshMaterial3d(standard),
            MeshMorphWeights::new(vec![0.5]).unwrap(),
            Transform::from_xyz(1.0, 0.0, 0.0),
            highlight,
        ));

        // once the pipelines are ready, a few more frames draw the glow with them
        compile_pipelines(&mut app);
        for _ in 0..5 {
            app.update();
        }

        let cache = app.sub_app(RenderApp).world().resource::<PipelineCache>();
        let labelled = |label: &str| -> Vec<RenderPipelineDescriptor> {
            cache
                .pipelines()
                .filter_map(|pipeline| match &pipeline.descriptor {
                    PipelineDescriptor::RenderPipelineDescriptor(descriptor)
                        if descriptor.label.as_deref() == Some(label) =>
                    {
                        Some((**descriptor).clone())
                    }
                    _ => None,
                })
                .collect()
        };
        let masks = labelled("highlight mask pipeline");
        let with = |def: &str| {
            masks
                .iter()
                .filter(|mask| mask.vertex.shader_defs.contains(&ShaderDefVal::from(def)))
                .count()
        };
        assert!(masks.len() >= 3, "only {} mask pipelines", masks.len());
        assert!(with("SKINNED") >= 1, "no skinned mask pipeline");
        assert!(with("MORPH_TARGETS") >= 1, "no morph-target mask pipeline");
        assert!(
            masks.len() > with("SKINNED") + with("MORPH_TARGETS"),
            "no plain mask pipeline"
        );
        assert!(!labelled("highlight blur pipeline").is_empty());
        assert!(!labelled("highlight composite pipeline").is_empty());
    }
}
