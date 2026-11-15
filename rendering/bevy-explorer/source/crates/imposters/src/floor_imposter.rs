use bevy::{
    asset::{AssetLoader, RenderAssetUsages},
    pbr::{ExtendedMaterial, MaterialExtension},
    prelude::*,
    render::render_resource::AsBindGroup,
};
use boimp::{
    asset_loader::ImposterLoader, bake::ImposterBakeMaterialExtension, render::Imposter,
    ImposterLoaderSettings,
};

#[derive(Clone, AsBindGroup, Asset, TypePath)]
pub struct FloorMaterialExt {
    #[uniform(100)]
    offset: f32,
}

impl MaterialExtension for FloorMaterialExt {
    type Base = Imposter;

    fn vertex_shader() -> bevy::render::render_resource::ShaderRef {
        "embedded://shaders/floor_vertex.wgsl".into()
    }

    fn prepass_vertex_shader() -> bevy::render::render_resource::ShaderRef {
        "embedded://shaders/floor_vertex.wgsl".into()
    }

    fn fragment_shader() -> bevy::render::render_resource::ShaderRef {
        "embedded://shaders/floor_fragment.wgsl".into()
    }

    fn prepass_fragment_shader() -> bevy::render::render_resource::ShaderRef {
        "embedded://shaders/floor_fragment.wgsl".into()
    }
}

impl ImposterBakeMaterialExtension for FloorMaterialExt {
    fn imposter_fragment_shader() -> bevy::render::render_resource::ShaderRef {
        "embedded://shaders/floor_bake.wgsl".into()
    }
}

pub type FloorImposter = ExtendedMaterial<FloorMaterialExt>;

#[derive(Default)]
pub struct FloorImposterLoader;

impl AssetLoader for FloorImposterLoader {
    type Asset = FloorImposter;
    type Settings = f32;
    type Error = anyhow::Error;

    async fn load(
        &self,
        reader: &mut dyn bevy::asset::io::Reader,
        settings: &Self::Settings,
        load_context: &mut bevy::asset::LoadContext<'_>,
    ) -> Result<Self::Asset, Self::Error> {
        let base = ImposterLoader
            .load(
                reader,
                &ImposterLoaderSettings {
                    multisample: true,
                    alpha_blend: 0.5,
                    transfer_priority: bevy::asset::RenderAssetTransferPriority::Immediate,
                    asset_usages: RenderAssetUsages::RENDER_WORLD,
                    ..Default::default()
                },
                load_context,
            )
            .await?;
        Ok(FloorImposter {
            base,
            extension: FloorMaterialExt { offset: *settings },
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// The floor imposter's softened sun shadow override (`floor_fragment.wgsl`) compiles on this
    /// machine's adapter, in a renderer with no window: a floor, spawned as the imposter renderer
    /// spawns it, in front of a camera with the prepasses and a shadowing light. Needs a GPU, so `#[ignore]`d and skipped by CI:
    ///
    /// ```text
    /// cargo test -p imposters floor_pipelines_compile -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs a GPU; run with --ignored"]
    fn floor_pipelines_compile() {
        use bevy::{
            core_pipeline::prepass::{DepthPrepass, NormalPrepass},
            pbr::{NotShadowCaster, NotShadowReceiver},
            render::{
                camera::RenderTarget,
                render_resource::{Extent3d, TextureDimension, TextureFormat},
            },
        };
        use boimp::{GridMode, ImposterData, ImposterRenderPlugin};
        use pipeline_test::{compile_pipelines, fragment_pipelines, headless_app, render_target};

        let mut app = headless_app();
        app.add_plugins((
            ImposterRenderPlugin,
            MaterialPlugin::<FloorImposter>::default(),
        ));
        app.finish();
        app.cleanup();
        app.update();

        let target = render_target(&mut app);
        let world = app.world_mut();
        // every imposter texture is an unsigned integer one
        let pixels = world.resource_mut::<Assets<Image>>().add(Image::new_fill(
            Extent3d::default(),
            TextureDimension::D2,
            &[0, 0, 0, 0],
            TextureFormat::R32Uint,
            RenderAssetUsages::RENDER_WORLD,
        ));
        let floor_mesh = world
            .resource_mut::<Assets<Mesh>>()
            .add(Plane3d::default().mesh().size(16.0, 16.0));
        // as `FloorImposterLoader` loads one: multisampled, alpha 0.5 as a mask
        let floor = world
            .resource_mut::<Assets<FloorImposter>>()
            .add(FloorImposter {
                base: Imposter {
                    data: ImposterData::new(
                        Vec3::ZERO,
                        8.0,
                        1,
                        64,
                        UVec2::ZERO,
                        UVec2::splat(64),
                        GridMode::Hemispherical,
                        true,
                        false,
                        1.0,
                        0.99,
                    ),
                    pixels: pixels.clone(),
                    indices: pixels.clone(),
                    indices_hi: pixels.clone(),
                    depth_palette: pixels,
                    alpha_mode: AlphaMode::Mask(0.5),
                    vram_bytes: 0,
                },
                extension: FloorMaterialExt { offset: 0.0 },
            });

        world.spawn((
            Camera3d::default(),
            Camera {
                target: RenderTarget::Image(target.into()),
                hdr: true,
                ..default()
            },
            DepthPrepass,
            NormalPrepass,
            Transform::from_xyz(0.0, 4.0, 8.0).looking_at(Vec3::ZERO, Vec3::Y),
        ));
        world.spawn((
            DirectionalLight {
                shadows_enabled: true,
                ..default()
            },
            Transform::from_xyz(1.0, 3.0, 1.0).looking_at(Vec3::ZERO, Vec3::Y),
        ));
        world.spawn((
            Mesh3d(floor_mesh),
            MeshMaterial3d(floor),
            // as the imposter renderer spawns floors
            NotShadowCaster,
            NotShadowReceiver,
            Transform::default(),
        ));

        compile_pipelines(&mut app);

        // the main pass and the prepass, both with floor_fragment.wgsl as the fragment stage
        let compiled = fragment_pipelines(&app, "embedded://shaders/floor_fragment.wgsl");
        println!("{compiled} floor_fragment.wgsl pipelines compiled");
        assert!(
            compiled >= 2,
            "only {compiled} floor_fragment.wgsl pipelines"
        );
    }
}
