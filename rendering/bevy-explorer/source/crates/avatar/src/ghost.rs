//! The ghost avatar: a hologram of the base body in the player's name colour, standing in its
//! idle, shown from the moment a player appears until their avatar has loaded, so nobody stands
//! invisible while their files are fetched. It appears bottom-up; once the avatar is built, the
//! two swap bottom-up: the avatar's meshes are clipped to a reveal height rising from the feet
//! (`reveal_mesh_tag`) while the ghost is clipped to the same height from the other side
//! (ghost_avatar.wgsl). A player gets one ghost, on their first appearance; a later change of look
//! rebuilds the avatar without one. Scene avatars and photo booths never get one.

use std::{collections::VecDeque, f32::consts::PI};

use bevy::{
    animation::{AnimationTarget, AnimationTargetId},
    gltf::{Gltf, GltfAssetLabel},
    pbr::NotShadowCaster,
    prelude::*,
    render::{
        mesh::MeshTag,
        render_resource::{AsBindGroup, ShaderRef},
    },
    scene::{InstanceId, SceneSpawner},
};
use common::structs::{AppConfig, PrimaryUser};
use comms::{global_crdt::ForeignPlayer, profile::UserProfile};
use scene_material::{
    reveal_mesh_tag, unrevealed_mesh_tag, SceneMaterial,
    SCENE_MATERIAL_REVEAL_FULL_HEIGHT as REVEAL_HEIGHT,
    SCENE_MATERIAL_REVEAL_HIDE_HEIGHT as HIDE_HEIGHT,
};

use crate::{mask_material::MaskMaterial, process_avatar, AvatarProcessed, AvatarSelection};

/// the ghost's bottom-up appearance
const REVEAL_DURATION: f32 = 0.8;
/// the bottom-up handover from the ghost to the avatar
const HANDOVER_DURATION: f32 = 0.5;

/// the content server's BaseMale body (`urn:decentraland:off-chain:base-avatars:BaseMale`),
/// without its textures and face masks, skinned to the "Avatar_*" armature the embedded
/// animations drive
const GHOST_BODY_PATH: &str = "embedded://avatars/ghost_body.glb";
/// the file's only animation is Idle_Male
const IDLE_PATH: &str = "embedded://animations/idle.glb";

/// the wearable bounds (`process_avatar`): the idle moves the body off its bind pose, so the
/// ghost's meshes are culled by these instead of their own extent
const AVATAR_BOUNDS_MIN: Vec3 = Vec3::new(-1.24, 0.0, -0.7);
const AVATAR_BOUNDS_MAX: Vec3 = Vec3::new(1.24, 2.42, 0.7);

pub struct GhostAvatarPlugin;

impl Plugin for GhostAvatarPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(MaterialPlugin::<GhostMaterial>::default());
        app.add_systems(Startup, load_ghost_body);
        app.add_systems(
            Update,
            (
                spawn_ghosts,
                dress_ghosts,
                clip_built_avatars.after(process_avatar),
                animate_ghosts,
            )
                .chain(),
        );
    }
}

#[derive(Asset, TypePath, AsBindGroup, Clone)]
pub struct GhostMaterial {
    /// the player's name colour
    #[uniform(0)]
    color: LinearRgba,
    /// x: the reveal height above the feet; y: 1 shows the ghost below it, -1 above it; z: 1
    /// fades the ghost near the camera like an avatar
    #[uniform(0)]
    reveal: Vec4,
}

impl Material for GhostMaterial {
    fn vertex_shader() -> ShaderRef {
        "embedded://shaders/ghost_avatar.wgsl".into()
    }

    fn fragment_shader() -> ShaderRef {
        "embedded://shaders/ghost_avatar.wgsl".into()
    }

    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Blend
    }
}

/// the body every ghost wears, and the idle it stands in
#[derive(Resource)]
struct GhostBody {
    gltf: Handle<Gltf>,
    idle: Handle<AnimationClip>,
}

fn load_ghost_body(mut commands: Commands, asset_server: Res<AssetServer>) {
    commands.insert_resource(GhostBody {
        gltf: asset_server.load(GHOST_BODY_PATH),
        idle: asset_server.load(GltfAssetLabel::Animation(0).from_asset(IDLE_PATH)),
    });
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GhostPhase {
    /// the ghost appears bottom-up
    Revealing,
    /// the ghost stands in for the avatar until it is built
    Visible,
    /// the ghost gives way to the avatar bottom-up
    HandingOver,
    /// done: the avatar shows
    Hidden,
}

/// the ghost's animation, apart from the ECS so a test can drive it
#[derive(Debug)]
struct GhostAnimation {
    phase: GhostPhase,
    elapsed: f32,
    /// the avatar is built and clipped away, waiting for the handover
    avatar_ready: bool,
    /// the reveal height both the ghost and the avatar are clipped to
    height: f32,
}

/// one tick's outcome
struct GhostStep {
    /// the ghost's reveal plane changed: its height, and whether the ghost shows below it
    ghost: Option<(f32, bool)>,
    /// the handover finished: the ghost goes, the avatar is unclipped
    finished: bool,
}

impl GhostAnimation {
    fn new() -> Self {
        Self {
            phase: GhostPhase::Revealing,
            elapsed: 0.0,
            avatar_ready: false,
            height: HIDE_HEIGHT,
        }
    }

    /// whether the ghost stands in: an avatar built now is clipped away until the handover
    fn standing_in(&self) -> bool {
        matches!(self.phase, GhostPhase::Revealing | GhostPhase::Visible)
    }

    /// the reveal height the avatar's meshes are clipped to; None once (or while never) unclipped
    fn avatar_reveal(&self) -> Option<f32> {
        match self.phase {
            GhostPhase::Revealing | GhostPhase::Visible => self.avatar_ready.then_some(HIDE_HEIGHT),
            GhostPhase::HandingOver => Some(self.height),
            GhostPhase::Hidden => None,
        }
    }

    /// the reveal height an avatar built now is clipped to: hidden while the ghost stands in,
    /// the current plane mid-handover so a look rebuilt then doesn't pop in above it; None once
    /// the ghost is gone
    fn built_avatar_reveal(&self) -> Option<f32> {
        self.avatar_reveal()
            .or_else(|| self.standing_in().then_some(HIDE_HEIGHT))
    }

    fn tick(&mut self, dt: f32) -> GhostStep {
        match self.phase {
            GhostPhase::Revealing => {
                self.elapsed += dt;
                let progress = (self.elapsed / REVEAL_DURATION).min(1.0);
                self.height = HIDE_HEIGHT + (REVEAL_HEIGHT - HIDE_HEIGHT) * progress;
                if progress >= 1.0 {
                    self.phase = GhostPhase::Visible;
                    self.elapsed = 0.0;
                }
                GhostStep {
                    ghost: Some((self.height, true)),
                    finished: false,
                }
            }
            GhostPhase::Visible => {
                if !self.avatar_ready {
                    return GhostStep {
                        ghost: None,
                        finished: false,
                    };
                }
                self.phase = GhostPhase::HandingOver;
                self.elapsed = 0.0;
                self.height = HIDE_HEIGHT;
                GhostStep {
                    ghost: Some((self.height, false)),
                    finished: false,
                }
            }
            GhostPhase::HandingOver => {
                self.elapsed += dt;
                let progress = (self.elapsed / HANDOVER_DURATION).min(1.0);
                self.height = HIDE_HEIGHT + (REVEAL_HEIGHT - HIDE_HEIGHT) * progress;
                if progress >= 1.0 {
                    self.phase = GhostPhase::Hidden;
                    return GhostStep {
                        ghost: None,
                        finished: true,
                    };
                }
                GhostStep {
                    ghost: Some((self.height, false)),
                    finished: false,
                }
            }
            GhostPhase::Hidden => GhostStep {
                ghost: None,
                finished: false,
            },
        }
    }
}

/// a player's ghost, kept once it is gone so they never get another
#[derive(Component)]
pub struct AvatarGhost {
    animation: GhostAnimation,
    /// the body entity under the player; the base body's scene spawns under it
    body: Entity,
    material: Handle<GhostMaterial>,
}

impl AvatarGhost {
    /// whether the ghost is on show: the nametag sits at a fixed height above it
    pub fn showing(&self) -> bool {
        self.animation.phase != GhostPhase::Hidden
    }
}

/// a ghost's body until it is dressed: the base body's scene spawns under it once the glTF is
/// in, then every mesh gets the ghost material and the armature the idle
#[derive(Component)]
struct GhostDressing {
    material: Handle<GhostMaterial>,
    /// the body's scene, once spawned
    scene: Option<InstanceId>,
}

/// an avatar mesh clipped to the reveal of this player's ghost
#[derive(Component)]
struct RevealedBy(Entity);

#[allow(clippy::type_complexity)]
fn spawn_ghosts(
    mut commands: Commands,
    players: Query<
        (Entity, &UserProfile, Has<PrimaryUser>),
        (
            Added<AvatarSelection>,
            Without<AvatarGhost>,
            Or<(With<ForeignPlayer>, With<PrimaryUser>)>,
        ),
    >,
    mut materials: ResMut<Assets<GhostMaterial>>,
    config: Res<AppConfig>,
) {
    if !config.graphics.avatar_ghosts {
        return;
    }

    for (player, profile, is_primary) in &players {
        let material = materials.add(GhostMaterial {
            color: profile.name_color().to_linear(),
            // the primary avatar doesn't dither either (`AvatarSelection::disable_dither`)
            reveal: Vec4::new(HIDE_HEIGHT, 1.0, if is_primary { 0.0 } else { 1.0 }, 0.0),
        });
        let body = commands
            .spawn((
                // the avatars' frame
                Transform::from_rotation(Quat::from_rotation_y(PI)),
                // until dressed
                Visibility::Hidden,
                GhostDressing {
                    material: material.clone(),
                    scene: None,
                },
                ChildOf(player),
            ))
            .id();
        commands.entity(player).try_insert(AvatarGhost {
            animation: GhostAnimation::new(),
            body,
            material,
        });
    }
}

/// spawns the base body under each new ghost, then dresses it: the ghost material on every
/// mesh, the idle clip on the armature, as `process_avatar` does for an avatar
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn dress_ghosts(
    mut commands: Commands,
    body: Res<GhostBody>,
    asset_server: Res<AssetServer>,
    gltfs: Res<Assets<Gltf>>,
    mut scene_spawner: ResMut<SceneSpawner>,
    mut bodies: Query<(Entity, &GlobalTransform, &mut GhostDressing)>,
    nodes: Query<(&Name, &ChildOf, Option<&GlobalTransform>, Has<Mesh3d>)>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
) {
    for (root, root_gt, mut dressing) in &mut bodies {
        let Some(scene) = dressing.scene else {
            if asset_server.load_state(&body.gltf).is_failed() {
                warn!("the ghost body didn't load: ghosts stay invisible");
                commands.entity(root).try_remove::<GhostDressing>();
                continue;
            }
            let Some(gltf) = gltfs.get(&body.gltf) else {
                continue;
            };
            match gltf.default_scene.as_ref() {
                Some(h_scene) => {
                    dressing.scene = Some(scene_spawner.spawn_as_child(h_scene.clone(), root));
                }
                None => {
                    warn!("the ghost body has no scene: ghosts stay invisible");
                    commands.entity(root).try_remove::<GhostDressing>();
                }
            }
            continue;
        };
        if !scene_spawner.instance_is_ready(scene)
            || !asset_server.is_loaded_with_dependencies(&body.idle)
        {
            continue;
        }

        let mut armature = None;
        let mut bones = Vec::new();
        for entity in scene_spawner.iter_instance_entities(scene) {
            let Ok((name, _, gt, has_mesh)) = nodes.get(entity) else {
                continue;
            };
            let name = name.to_lowercase();
            if name == "armature" {
                armature = Some(entity);
            } else if name.starts_with("avatar_") {
                bones.push(entity);
            }
            if has_mesh {
                commands
                    .entity(entity)
                    .try_remove::<MeshMaterial3d<StandardMaterial>>()
                    .try_insert((MeshMaterial3d(dressing.material.clone()), NotShadowCaster));
                if let Some(gt) = gt {
                    commands
                        .entity(entity)
                        .try_insert(crate::calculate_local_aabb(
                            AVATAR_BOUNDS_MIN + root_gt.translation(),
                            AVATAR_BOUNDS_MAX + root_gt.translation(),
                            gt,
                        ));
                }
            }
        }

        if let Some(armature) = armature {
            // the clip's targets are named by their path from the armature
            for bone in bones {
                let mut path = VecDeque::new();
                let mut entity = bone;
                while let Ok((name, parent, ..)) = nodes.get(entity) {
                    path.push_front(name);
                    if entity == armature {
                        break;
                    }
                    entity = parent.parent();
                }
                commands.entity(bone).try_insert(AnimationTarget {
                    id: AnimationTargetId::from_names(path.into_iter()),
                    player: root,
                });
            }
            commands.entity(armature).try_insert(AnimationTarget {
                id: AnimationTargetId::from_name(&Name::new("Armature")),
                player: root,
            });
            let (graph, idle) = AnimationGraph::from_clip(body.idle.clone());
            let mut player = AnimationPlayer::default();
            player.play(idle).repeat();
            commands
                .entity(root)
                .try_insert((player, AnimationGraphHandle(graphs.add(graph))));
        } else {
            warn!("the ghost body has no armature: ghosts stand still");
        }

        commands
            .entity(root)
            .try_insert(Visibility::Inherited)
            .try_remove::<GhostDressing>();
    }
}

// an avatar built while its ghost stands in is clipped away, to be revealed by the handover; one
// rebuilt mid-handover (a look change) joins the reveal at its current height
#[allow(clippy::type_complexity)]
fn clip_built_avatars(
    mut commands: Commands,
    built: Query<(Entity, &ChildOf), Added<AvatarProcessed>>,
    mut ghosts: Query<&mut AvatarGhost>,
    children: Query<&Children>,
    mut meshes: Query<
        (&mut Mesh3d, &mut MeshTag),
        Or<(
            With<MeshMaterial3d<SceneMaterial>>,
            With<MeshMaterial3d<MaskMaterial>>,
        )>,
    >,
) {
    for (avatar, parent) in &built {
        let player = parent.parent();
        let Ok(mut ghost) = ghosts.get_mut(player) else {
            continue;
        };
        let Some(height) = ghost.animation.built_avatar_reveal() else {
            continue;
        };

        let mut clipped = false;
        for entity in children.iter_descendants(avatar) {
            let Ok((mut mesh, mut tag)) = meshes.get_mut(entity) else {
                continue;
            };
            tag.0 = reveal_mesh_tag(tag.0, height);
            // bevy's mesh extraction doesn't watch MeshTag (0.16); Mesh3d is on its list
            mesh.set_changed();
            commands.entity(entity).try_insert(RevealedBy(player));
            clipped = true;
        }
        // an avatar with nothing to show keeps its ghost
        if clipped {
            ghost.animation.avatar_ready = true;
        }
    }
}

fn animate_ghosts(
    mut commands: Commands,
    time: Res<Time>,
    mut ghosts: Query<&mut AvatarGhost>,
    mut materials: ResMut<Assets<GhostMaterial>>,
    mut revealed: Query<(Entity, &mut Mesh3d, &mut MeshTag, &RevealedBy)>,
) {
    let dt = time.delta_secs();
    for mut ghost in &mut ghosts {
        let step = ghost.animation.tick(dt);
        if let Some((height, below)) = step.ghost {
            if let Some(material) = materials.get_mut(&ghost.material) {
                material.reveal.x = height;
                material.reveal.y = if below { 1.0 } else { -1.0 };
            }
        }
        if step.finished {
            commands.entity(ghost.body).try_despawn();
        }
    }

    for (entity, mut mesh, mut tag, revealed_by) in &mut revealed {
        let reveal = ghosts
            .get(revealed_by.0)
            .ok()
            .and_then(|ghost| ghost.animation.avatar_reveal());
        let changed = match reveal {
            Some(height) => {
                let revealed = reveal_mesh_tag(tag.0, height);
                tag.set_if_neq(MeshTag(revealed))
            }
            None => {
                tag.0 = unrevealed_mesh_tag(tag.0);
                commands.entity(entity).try_remove::<RevealedBy>();
                true
            }
        };
        // bevy's mesh extraction doesn't watch MeshTag (0.16); Mesh3d is on its list
        if changed {
            mesh.set_changed();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use bevy::asset::LoadState;

    use super::*;

    const FRAME: f32 = 0.05;

    /// ticks the animation through `seconds` of frames
    fn run(animation: &mut GhostAnimation, seconds: f32) -> Vec<GhostStep> {
        let frames = (seconds / FRAME).round() as usize;
        (0..frames).map(|_| animation.tick(FRAME)).collect()
    }

    #[test]
    fn reveals_bottom_up_then_stands_in() {
        let mut animation = GhostAnimation::new();
        assert!(animation.standing_in());

        let steps = run(&mut animation, REVEAL_DURATION / 2.0);
        assert_eq!(animation.phase, GhostPhase::Revealing);
        let (height, below) = steps.last().unwrap().ghost.unwrap();
        assert!(below);
        assert!(height > HIDE_HEIGHT && height < REVEAL_HEIGHT);

        // a frame past the duration: the reveal is complete
        run(&mut animation, REVEAL_DURATION / 2.0 + FRAME);
        assert_eq!(animation.phase, GhostPhase::Visible);
        assert!((animation.height - REVEAL_HEIGHT).abs() < 1e-4);

        // nothing to hand over to: the ghost stays, and nothing is clipped
        let steps = run(&mut animation, 10.0);
        assert_eq!(animation.phase, GhostPhase::Visible);
        assert!(steps
            .iter()
            .all(|step| step.ghost.is_none() && !step.finished));
        assert_eq!(animation.avatar_reveal(), None);
    }

    #[test]
    fn avatar_built_during_the_reveal_waits_clipped_for_the_handover() {
        let mut animation = GhostAnimation::new();
        run(&mut animation, 0.2);
        animation.avatar_ready = true;
        assert_eq!(animation.avatar_reveal(), Some(HIDE_HEIGHT));

        // the reveal completes, then the handover starts at once from the feet, the ghost now
        // showing above the plane
        let steps = run(&mut animation, REVEAL_DURATION - 0.2 + FRAME);
        assert_eq!(animation.phase, GhostPhase::HandingOver);
        let handover_start = steps
            .iter()
            .find(|step| step.ghost.is_some_and(|(_, below)| !below))
            .unwrap();
        assert_eq!(handover_start.ghost, Some((HIDE_HEIGHT, false)));
        assert_eq!(animation.avatar_reveal(), Some(animation.height));
    }

    #[test]
    fn hands_over_bottom_up_then_finishes_once() {
        let mut animation = GhostAnimation::new();
        run(&mut animation, REVEAL_DURATION + FRAME);
        animation.avatar_ready = true;
        animation.tick(0.0);

        let steps = run(&mut animation, HANDOVER_DURATION / 2.0);
        let (ghost_height, below) = steps.last().unwrap().ghost.unwrap();
        assert!(!below);
        assert_eq!(animation.avatar_reveal(), Some(ghost_height));
        assert!(ghost_height > HIDE_HEIGHT && ghost_height < REVEAL_HEIGHT);

        let steps = run(&mut animation, HANDOVER_DURATION / 2.0 + FRAME);
        assert_eq!(animation.phase, GhostPhase::Hidden);
        assert_eq!(steps.iter().filter(|step| step.finished).count(), 1);
        assert_eq!(animation.avatar_reveal(), None);
        assert!(!animation.standing_in());

        let steps = run(&mut animation, 1.0);
        assert!(steps
            .iter()
            .all(|step| step.ghost.is_none() && !step.finished));
    }

    #[test]
    fn avatar_rebuilt_mid_handover_joins_the_reveal() {
        let mut animation = GhostAnimation::new();
        // while the ghost stands in, a built avatar is clipped away
        assert_eq!(animation.built_avatar_reveal(), Some(HIDE_HEIGHT));
        run(&mut animation, REVEAL_DURATION + FRAME);
        assert_eq!(animation.built_avatar_reveal(), Some(HIDE_HEIGHT));
        animation.avatar_ready = true;
        animation.tick(0.0);

        // mid-handover, a rebuilt avatar is clipped to the plane as it stands
        run(&mut animation, HANDOVER_DURATION / 2.0);
        let height = animation.built_avatar_reveal().unwrap();
        assert!(height > HIDE_HEIGHT && height < REVEAL_HEIGHT);
        assert_eq!(animation.avatar_reveal(), Some(height));

        // once the ghost is gone, nothing is clipped
        run(&mut animation, HANDOVER_DURATION / 2.0 + FRAME);
        assert_eq!(animation.built_avatar_reveal(), None);
    }

    /// runs the app until the ghost body and the idle clip are loaded
    fn load_ghost_body_in(app: &mut App) {
        let started = Instant::now();
        loop {
            app.update();
            let server = app.world().resource::<AssetServer>();
            let body = app.world().resource::<GhostBody>();
            let states = [server.load_state(&body.gltf), server.load_state(&body.idle)];
            if states.iter().all(LoadState::is_loaded) {
                return;
            }
            if let Some(failed) = states.iter().find(|state| state.is_failed()) {
                panic!("{failed:?}");
            }
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "loading timed out"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The embedded body loads without a renderer, carries no textures or face masks, and its
    /// armature has every bone the idle clip animates.
    #[test]
    fn ghost_body_has_the_idle_bones() {
        use bevy::{
            asset::AssetPlugin,
            gltf::{GltfNode, GltfPlugin},
            platform::collections::HashSet,
            render::mesh::skinning::SkinnedMeshInverseBindposes,
            scene::ScenePlugin,
        };

        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            assets::EmbedAssetsPlugin,
            ScenePlugin,
            GltfPlugin::default(),
        ))
        .init_asset::<Mesh>()
        .init_asset::<Image>()
        .init_asset::<StandardMaterial>()
        .init_asset::<SkinnedMeshInverseBindposes>()
        .init_asset::<AnimationClip>()
        .add_systems(Startup, load_ghost_body);
        app.finish();
        app.cleanup();
        load_ghost_body_in(&mut app);

        let world = app.world();
        let body = world.resource::<GhostBody>();
        let gltf = world.resource::<Assets<Gltf>>().get(&body.gltf).unwrap();
        assert!(gltf.default_scene.is_some());
        assert!(gltf.materials.is_empty(), "the ghost material dresses it");

        let nodes = world.resource::<Assets<GltfNode>>();
        let armature = nodes.get(&gltf.named_nodes["Armature"]).unwrap();
        let mut meshes = 0;
        let mut ids = HashSet::new();
        let mut pending = vec![(armature, vec![Name::new("Armature")])];
        while let Some((node, path)) = pending.pop() {
            assert!(!node.name.to_lowercase().contains("mask"), "{}", node.name);
            meshes += node.mesh.is_some() as usize;
            ids.insert(AnimationTargetId::from_names(path.iter()));
            for child in &node.children {
                let child = nodes.get(child).unwrap();
                let mut path = path.clone();
                path.push(Name::new(child.name.clone()));
                pending.push((child, path));
            }
        }
        // feet, hands, head, lower and upper body
        assert_eq!(meshes, 5);

        let idle = world
            .resource::<Assets<AnimationClip>>()
            .get(&body.idle)
            .unwrap();
        assert!(
            idle.curves().len() > 50,
            "{} animated bones",
            idle.curves().len()
        );
        let missing = idle.curves().keys().filter(|id| !ids.contains(*id)).count();
        assert_eq!(
            missing, 0,
            "{missing} animated bones are not in the ghost body"
        );
    }

    /// The ghost's shader, and the avatar shaders with the reveal clip, compile on this machine's
    /// adapter, in a renderer with no window: a dressed ghost (the skinned body in the ghost
    /// material), a revealed scene-material mesh and a revealed mask mesh in front of a camera
    /// with the prepasses and a shadowing light. Needs a GPU, so `#[ignore]`d and skipped by CI:
    ///
    /// ```text
    /// cargo test -p avatar ghost_and_reveal_shaders -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs a GPU; run with --ignored"]
    fn ghost_and_reveal_shaders_compile() {
        use bevy::{
            asset::RenderAssetUsages,
            core_pipeline::prepass::{DepthPrepass, NormalPrepass},
            image::Image,
            render::{
                camera::RenderTarget,
                render_resource::{Extent3d, TextureDimension, TextureFormat},
                storage::ShaderStorageBuffer,
            },
        };
        use pipeline_test::{compile_pipelines, headless_app, render_target};
        use scene_material::{
            SceneBound, SceneBounds, EMPTY_SCENE_CELLS, SCENE_MATERIAL_CONE_ONLY_DITHER_MESH_TAG,
            SCENE_MATERIAL_TOON_MESH_TAG,
        };

        let mut app = headless_app();
        app.add_plugins((
            MaterialPlugin::<SceneMaterial>::default(),
            crate::mask_material::MaskMaterialPlugin,
            GhostAvatarPlugin,
        ));
        // spawn_ghosts reads the ghosts setting
        app.insert_resource(AppConfig::default());
        app.finish();
        app.cleanup();
        load_ghost_body_in(&mut app);

        let target = render_target(&mut app);
        let world = app.world_mut();
        // the unbounded scene's cell table, as SceneBoundPlugin inserts it
        world.resource_mut::<Assets<ShaderStorageBuffer>>().insert(
            EMPTY_SCENE_CELLS.id(),
            ShaderStorageBuffer::new(&[0; 8], RenderAssetUsages::RENDER_WORLD),
        );
        let texture = world.resource_mut::<Assets<Image>>().add(Image::new_fill(
            Extent3d::default(),
            TextureDimension::D2,
            &[255, 255, 255, 255],
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::all(),
        ));
        let cube = world
            .resource_mut::<Assets<Mesh>>()
            .add(Cuboid::new(0.5, 1.0, 0.5));
        let ghost = world
            .resource_mut::<Assets<GhostMaterial>>()
            .add(GhostMaterial {
                color: LinearRgba::BLUE,
                reveal: Vec4::new(1.0, 1.0, 1.0, 0.0),
            });
        let scene = world
            .resource_mut::<Assets<SceneMaterial>>()
            .add(SceneMaterial {
                base: StandardMaterial::default(),
                extension: SceneBound::new_unbounded(),
            });
        let mask = world
            .resource_mut::<Assets<MaskMaterial>>()
            .add(MaskMaterial::new(
                Color::WHITE,
                texture.clone(),
                texture,
                &SceneBounds::unbounded(),
                0.0,
            ));

        world.spawn((
            Camera3d::default(),
            Camera {
                target: RenderTarget::Image(target.into()),
                ..default()
            },
            DepthPrepass,
            NormalPrepass,
            Transform::from_xyz(0.0, 1.0, 4.0).looking_at(Vec3::Y, Vec3::Y),
        ));
        world.spawn((
            DirectionalLight {
                shadows_enabled: true,
                ..default()
            },
            Transform::from_xyz(1.0, 3.0, 1.0).looking_at(Vec3::ZERO, Vec3::Y),
        ));
        // a ghost, as spawn_ghosts spawns one; dress_ghosts dresses it
        let body = world
            .spawn((
                Transform::default(),
                Visibility::Hidden,
                GhostDressing {
                    material: ghost,
                    scene: None,
                },
            ))
            .id();
        let revealed = reveal_mesh_tag(
            SCENE_MATERIAL_TOON_MESH_TAG | SCENE_MATERIAL_CONE_ONLY_DITHER_MESH_TAG,
            1.0,
        );
        world.spawn((
            Mesh3d(cube.clone()),
            MeshMaterial3d(scene),
            MeshTag(revealed),
            Transform::from_xyz(-1.0, 0.5, 0.0),
        ));
        world.spawn((
            Mesh3d(cube),
            MeshMaterial3d(mask),
            MeshTag(revealed),
            Transform::from_xyz(1.0, 0.5, 0.0),
        ));

        let started = Instant::now();
        while app.world().get::<AnimationPlayer>(body).is_none() {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "the ghost never dressed"
            );
            app.update();
        }
        // a few frames queue every pipeline; then they compile in the background
        let ok = compile_pipelines(&mut app);
        // the skinned ghost, the scene material's main, prepass and shadow passes, the mask
        // material
        assert!(ok >= 5, "only {ok} pipelines compiled");
    }
}
