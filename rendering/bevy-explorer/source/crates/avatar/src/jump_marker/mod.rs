//! The jump landing marker: a soft dark blob on the ground under the player while airborne,
//! showing where they will land. A box under the primary player, from just below its feet to
//! below the ground, projects the blob onto whatever surface the depth prepass has inside it, so
//! it follows slopes, steps and ledges. Its opacity and scale follow the distance to the ground,
//! computed here; jump_marker.wgsl draws the blob.

mod diagnostics;

use bevy::{
    app::Propagate,
    pbr::{MaterialPipeline, MaterialPipelineKey, NotShadowCaster, NotShadowReceiver},
    prelude::*,
    render::{
        mesh::MeshVertexBufferLayoutRef,
        render_resource::{
            AsBindGroup, CompareFunction, Face, RenderPipelineDescriptor, ShaderRef,
            SpecializedMeshPipelineError,
        },
        view::RenderLayers,
    },
};
use common::{
    sets::{PostUpdateSets, SetupSets},
    structs::{AvatarDynamicState, PrimaryUser},
};

/// the marker's footprint on the ground at full scale, in metres
pub const MARKER_SIZE: Vec2 = Vec2::new(2.0, 1.5);
/// the opacity range and the ground distances it follows: it rises over the first pair of
/// distances, so the blob fades in just above the grounded height, and falls over the second
pub const OPACITY_RANGE: Vec2 = Vec2::new(0.0, 0.9);
pub const OPACITY_DISTANCES: Vec4 = Vec4::new(GROUNDED_HEIGHT, 0.4, 6.0, 10.0);
/// scales the whole opacity range, keeping the blob a light cue rather than a hole in the ground
pub const FADE_FACTOR: f32 = 0.5;
/// the blob's scale range and the distances it follows: small near the ground, reaching full
/// size at the last distance
pub const HEIGHT_SCALE_RANGE: Vec2 = Vec2::new(0.4, 1.0);
pub const HEIGHT_SCALE_DISTANCES: Vec4 = Vec4::new(0.0, 0.0, 4.0, 8.0);
/// the ground probe's range: farther from the ground there is no marker
pub const MAX_GROUND_DISTANCE: f32 = 10.0;
/// at or below this distance to the ground the player is standing, not airborne: the threshold
/// the animation, position broadcast and analytics use, so the blob shows exactly when the jump
/// or fall animation plays and stepping down stairs does not flash it
pub const GROUNDED_HEIGHT: f32 = 0.2;
/// how far below the ground under the player's centre the box reaches, so the blob still lands
/// on ground that falls away within the footprint: a 45 degree slope at full size
pub const PROJECTION_DEPTH: f32 = 1.0;
/// the gap between the player's feet and the top of the box, so the blob never darkens the
/// avatar; below the grounded height, so the box always has a height while shown
const FEET_CLEARANCE: f32 = 0.1;

/// smoothstep over `[from, to]`; a degenerate range is a step at `from`
fn ramp(from: f32, to: f32, x: f32) -> f32 {
    if to <= from {
        return (x >= from) as u32 as f32;
    }
    let t = ((x - from) / (to - from)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// one over the first pair of `distances`, back to zero over the second
fn rise_and_fall(distances: Vec4, ground_distance: f32) -> f32 {
    ramp(distances.x, distances.y, ground_distance)
        - ramp(distances.z, distances.w, ground_distance)
}

/// Opacity of the blob for a ground distance in metres: rises over the first pair of distances,
/// falls over the second, scaled by the fade factor.
pub fn marker_opacity(ground_distance: f32) -> f32 {
    FADE_FACTOR
        * (OPACITY_RANGE.x
            + (OPACITY_RANGE.y - OPACITY_RANGE.x)
                * rise_and_fall(OPACITY_DISTANCES, ground_distance))
}

/// Scale of the blob for a ground distance in metres: small near the ground, growing over the
/// second pair of distances.
pub fn marker_scale(ground_distance: f32) -> f32 {
    HEIGHT_SCALE_RANGE.x
        + (HEIGHT_SCALE_RANGE.y - HEIGHT_SCALE_RANGE.x)
            * (1.0 - rise_and_fall(HEIGHT_SCALE_DISTANCES, ground_distance))
}

/// the projection box under the player; a child of it
#[derive(Component)]
pub struct JumpMarker;

#[derive(Asset, TypePath, AsBindGroup, Clone)]
pub struct JumpMarkerMaterial {
    /// the blob's opacity at its centre, from [`marker_opacity`]
    #[uniform(0)]
    pub opacity: f32,
}

impl Material for JumpMarkerMaterial {
    fn fragment_shader() -> ShaderRef {
        "embedded://shaders/jump_marker.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Blend
    }

    /// draws the box's far faces with no depth test, so the blob shows with the camera inside
    /// the box or the box through the ground; the shader tests the surface behind each pixel
    fn specialize(
        _: &MaterialPipeline<Self>,
        descriptor: &mut RenderPipelineDescriptor,
        _: &MeshVertexBufferLayoutRef,
        _: MaterialPipelineKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        descriptor.primitive.cull_mode = Some(Face::Front);
        if let Some(depth_stencil) = descriptor.depth_stencil.as_mut() {
            depth_stencil.depth_compare = CompareFunction::Always;
        }
        Ok(())
    }
}

pub struct JumpMarkerPlugin;

impl Plugin for JumpMarkerPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(MaterialPlugin::<JumpMarkerMaterial>::default());
        app.insert_resource(JumpMarkerEnabled(true));
        app.add_plugins(diagnostics::JumpMarkerDiagnosticsPlugin);
        app.add_systems(Startup, spawn_jump_marker.in_set(SetupSets::Main));
        // after the movement writes this frame's position and ground distance, before the
        // transforms propagate, so the box sits over the ground the player is over this frame
        app.add_systems(
            PostUpdate,
            update_jump_marker
                .after(PostUpdateSets::PlayerUpdate)
                .before(PostUpdateSets::CameraUpdate),
        );
    }
}

/// Whether the marker shows while the player is airborne.
#[derive(Resource)]
pub struct JumpMarkerEnabled(pub bool);

/// The box's transform, local to the player, for a ground distance in metres: the blob's
/// footprint across, from the clearance under the feet down to the projection depth below the
/// ground. A unit cube scaled to this is the projection volume.
pub fn marker_transform(ground_distance: f32) -> Transform {
    let top = -FEET_CLEARANCE;
    let bottom = -(ground_distance + PROJECTION_DEPTH);
    let footprint = MARKER_SIZE * marker_scale(ground_distance);
    Transform::from_xyz(0.0, (top + bottom) / 2.0, 0.0).with_scale(Vec3::new(
        footprint.x,
        top - bottom,
        footprint.y,
    ))
}

fn spawn_jump_marker(
    mut commands: Commands,
    player: Single<Entity, With<PrimaryUser>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<JumpMarkerMaterial>>,
) {
    commands.spawn((
        JumpMarker,
        Mesh3d(meshes.add(Cuboid::new(1.0, 1.0, 1.0))),
        MeshMaterial3d(materials.add(JumpMarkerMaterial { opacity: 0.0 })),
        Transform::default(),
        Visibility::Hidden,
        // stays on the primary camera's layer: first person takes layer 0 off the player to
        // hide the body, which is the view where the marker matters most
        Propagate(RenderLayers::default()),
        NotShadowCaster,
        NotShadowReceiver,
        ChildOf(*player),
    ));
}

/// Fits the box under the player and shows it while the player is airborne.
/// `ground_height` is the player's distance above the ground (the movement system's downward
/// shape cast, not a world height): 0 standing, whatever the platform's height.
/// Shown, it inherits the player's visibility, so it hides with a hidden or out-of-world player.
#[allow(clippy::type_complexity)]
fn update_jump_marker(
    player: Single<&AvatarDynamicState, With<PrimaryUser>>,
    marker: Single<
        (
            &mut Transform,
            &mut Visibility,
            &MeshMaterial3d<JumpMarkerMaterial>,
        ),
        (With<JumpMarker>, Without<PrimaryUser>),
    >,
    mut materials: ResMut<Assets<JumpMarkerMaterial>>,
    enabled: Res<JumpMarkerEnabled>,
) {
    let state = player.into_inner();
    let (mut transform, mut visibility, material) = marker.into_inner();

    let ground_distance = state.ground_height;
    let airborne = ground_distance > GROUNDED_HEIGHT && ground_distance < MAX_GROUND_DISTANCE;
    let shown = enabled.0 && airborne;
    visibility.set_if_neq(if shown {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    });
    if !shown {
        return;
    }

    transform.set_if_neq(marker_transform(ground_distance));
    let opacity = marker_opacity(ground_distance);
    if materials
        .get(&material.0)
        .is_some_and(|m| m.opacity != opacity)
    {
        if let Some(m) = materials.get_mut(&material.0) {
            m.opacity = opacity;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_box_spans_from_the_feet_to_below_the_ground() {
        let distance = 3.0;
        let transform = marker_transform(distance);
        let half_height = transform.scale.y / 2.0;
        let top = transform.translation.y + half_height;
        let bottom = transform.translation.y - half_height;
        assert!((top + FEET_CLEARANCE).abs() < 1e-6, "{top}");
        assert!(
            (bottom + distance + PROJECTION_DEPTH).abs() < 1e-6,
            "{bottom}"
        );
        // the footprint across, scaled with the blob
        let footprint = MARKER_SIZE * marker_scale(distance);
        assert_eq!(transform.scale.xz(), footprint);
        // just above the grounded height the box still has a height
        assert!(marker_transform(GROUNDED_HEIGHT).scale.y > 0.0);
    }

    #[test]
    fn marker_ramps_follow_the_material_ranges() {
        // opacity rises 0 to 0.9 over 0.2..0.4 m and falls to 0 over 6..10 m, halved by the
        // fade factor
        assert_eq!(marker_opacity(0.0), 0.0);
        assert_eq!(marker_opacity(GROUNDED_HEIGHT), 0.0);
        assert!(marker_opacity(0.3) > 0.0 && marker_opacity(0.3) < 0.45);
        assert!((marker_opacity(0.4) - 0.45).abs() < 1e-6);
        assert!((marker_opacity(1.0) - 0.45).abs() < 1e-6);
        assert!((marker_opacity(3.0) - 0.45).abs() < 1e-6);
        assert_eq!(marker_opacity(10.0), 0.0);
        // the blob is small near the ground and grows to full size over 4..8 m
        assert!((marker_scale(0.5) - 0.4).abs() < 1e-6);
        assert!((marker_scale(4.0) - 0.4).abs() < 1e-6);
        assert!((marker_scale(8.0) - 1.0).abs() < 1e-6);
        assert!(marker_scale(6.0) > 0.4 && marker_scale(6.0) < 1.0);
    }

    fn test_app() -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins(AssetPlugin::default());
        app.init_asset::<Mesh>();
        app.init_asset::<JumpMarkerMaterial>();
        let player = app
            .world_mut()
            .spawn((
                PrimaryUser::default(),
                Transform::from_xyz(0.0, 5.0, 0.0),
                AvatarDynamicState::default(),
            ))
            .id();
        app.insert_resource(JumpMarkerEnabled(true));
        app.add_systems(Startup, spawn_jump_marker);
        app.add_systems(Update, update_jump_marker);
        app.update();
        (app, player)
    }

    /// the marker's visibility, local transform, material opacity and parent
    fn marker_state(app: &mut App) -> (Visibility, Transform, f32, Entity) {
        let world = app.world_mut();
        let (visibility, transform, material, parent) = {
            let (visibility, transform, material, child_of) = world
                .query_filtered::<(
                    &Visibility,
                    &Transform,
                    &MeshMaterial3d<JumpMarkerMaterial>,
                    &ChildOf,
                ), With<JumpMarker>>()
                .single(world)
                .unwrap();
            (
                *visibility,
                *transform,
                material.0.clone(),
                child_of.parent(),
            )
        };
        let opacity = world
            .resource::<Assets<JumpMarkerMaterial>>()
            .get(&material)
            .unwrap()
            .opacity;
        (visibility, transform, opacity, parent)
    }

    #[test]
    fn marker_shows_under_an_airborne_player_only() {
        let (mut app, player) = test_app();

        let set_ground_distance = |app: &mut App, distance: f32| {
            app.world_mut()
                .get_mut::<AvatarDynamicState>(player)
                .unwrap()
                .ground_height = distance;
            app.update();
        };

        // standing (on a 5 m platform: the player's height is not the distance)
        set_ground_distance(&mut app, 0.0);
        let (visibility, _, _, parent) = marker_state(&mut app);
        assert_eq!(visibility, Visibility::Hidden);
        assert_eq!(parent, player, "the marker is a child of the player");
        // its own render layers, so first person's layer swap on the player stops at it
        let world = app.world_mut();
        let layers = world
            .query_filtered::<&Propagate<RenderLayers>, With<JumpMarker>>()
            .single(world)
            .unwrap();
        assert_eq!(layers.0, RenderLayers::default());

        // airborne, the ground 6 m below: following the player's own visibility
        set_ground_distance(&mut app, 6.0);
        let (visibility, transform, opacity, _) = marker_state(&mut app);
        assert_eq!(visibility, Visibility::Inherited);
        assert_eq!(opacity, marker_opacity(6.0));
        assert_eq!(transform, marker_transform(6.0));

        // too high for the marker
        set_ground_distance(&mut app, MAX_GROUND_DISTANCE + 2.0);
        assert_eq!(marker_state(&mut app).0, Visibility::Hidden);

        // stepping down a stair: within the grounded height the walk animation keeps playing
        set_ground_distance(&mut app, 0.15);
        assert_eq!(marker_state(&mut app).0, Visibility::Hidden);

        // disabled from the debug panel: hidden while airborne, back once enabled
        app.world_mut().resource_mut::<JumpMarkerEnabled>().0 = false;
        set_ground_distance(&mut app, 3.0);
        assert_eq!(marker_state(&mut app).0, Visibility::Hidden);
        app.world_mut().resource_mut::<JumpMarkerEnabled>().0 = true;
        app.update();
        assert_eq!(marker_state(&mut app).0, Visibility::Inherited);
    }

    /// The marker's shader compiles on this machine's adapter, in a renderer with no window: the
    /// box in front of a camera with the depth prepass it reads. Needs a GPU, so `#[ignore]`d and skipped by CI:
    ///
    /// ```text
    /// cargo test -p avatar jump_marker_pipeline_compiles -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs a GPU; run with --ignored"]
    fn jump_marker_pipeline_compiles() {
        use bevy::render::camera::RenderTarget;
        use pipeline_test::{compile_pipelines, headless_app, render_target};

        let mut app = headless_app();
        app.add_plugins((
            console::ConsolePlugin {
                add_bevy_console: false,
            },
            JumpMarkerPlugin,
        ));
        app.finish();
        app.cleanup();

        let target = render_target(&mut app);
        let world = app.world_mut();
        world.spawn((
            Camera3d::default(),
            bevy::core_pipeline::prepass::DepthPrepass,
            Camera {
                target: RenderTarget::Image(target.into()),
                ..default()
            },
            Transform::from_xyz(0.0, 3.0, 4.0).looking_at(Vec3::ZERO, Vec3::Y),
        ));
        // a visible marker, as update_jump_marker leaves it while airborne
        let mesh = world
            .resource_mut::<Assets<Mesh>>()
            .add(Cuboid::new(1.0, 1.0, 1.0));
        let material = world
            .resource_mut::<Assets<JumpMarkerMaterial>>()
            .add(JumpMarkerMaterial {
                opacity: marker_opacity(3.0),
            });
        world.spawn((
            JumpMarker,
            Mesh3d(mesh),
            MeshMaterial3d(material),
            marker_transform(3.0),
            Visibility::Visible,
        ));

        // a few frames queue the pipeline; then it compiles in the background
        let ok = compile_pipelines(&mut app);
        assert!(ok >= 1, "no pipeline compiled");
    }
}
