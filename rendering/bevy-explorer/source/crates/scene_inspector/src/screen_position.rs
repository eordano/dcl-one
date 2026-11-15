//! `/entity_screen_position`: where a scene entity is in the 3D view, in the window coordinates
//! `/click_at` takes, so an end-to-end test can click a scene object.

use bevy::prelude::*;
use bevy::render::camera::ViewportConversionError;
use bevy::render::primitives::Aabb;
use bevy_console::ConsoleCommand;
use common::structs::PrimaryCamera;
use console::{DoAddConsoleCommand, PendingConsoleResponses};
use dcl_component::{SceneComponentId, SceneEntityId};
use dcl_types::interface::CrdtStore;
use scene_runner::update_scene::synthetic_pointer::{anchor_point, Anchor};

use crate::{
    active_scene::SceneResolver, read_commands::parse_entity_id, snapshot::PendingSnapshotRequests,
};

pub struct ScreenPositionCommandPlugin;

impl Plugin for ScreenPositionCommandPlugin {
    fn build(&self, app: &mut App) {
        app.add_console_command::<EntityScreenPositionCommand, _>(entity_screen_position_cmd);
    }
}

/// Report, as JSON, where a scene entity of the active scene is on screen, in logical window
/// pixels from the top left, as /click_at takes them: `x` and `y` (null while the entity is behind
/// the camera), `onScreen`, `inFront`, `depth` along the view direction, `distance` from the
/// camera, the `anchor` aimed at and the camera's `viewport`. The entity is named as in
/// /entity_components, and a click needs the point on screen and not under the HUD
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/entity_screen_position")]
struct EntityScreenPositionCommand {
    /// Entity id (u32, as /scene_entities lists it) or alias: root, player, camera
    entity: String,
    /// Aim at the centre of the bounds of the meshes of the entity and everything parented under
    /// it, rather than at the entity's origin
    #[arg(long)]
    bounds: bool,
}

#[allow(clippy::too_many_arguments)]
fn entity_screen_position_cmd(
    mut input: ConsoleCommand<EntityScreenPositionCommand>,
    resolver: SceneResolver,
    camera: Query<(&Camera, &GlobalTransform), With<PrimaryCamera>>,
    transforms: Query<&GlobalTransform>,
    meshes: Query<(&Aabb, &GlobalTransform, &InheritedVisibility)>,
    children: Query<&Children>,
    mut pending: ResMut<PendingSnapshotRequests>,
    mut console_responses: ResMut<PendingConsoleResponses>,
) {
    let Some(Ok(cmd)) = input.take() else {
        return;
    };
    let eid = match parse_entity_id(&cmd.entity) {
        Ok(eid) => eid,
        Err(e) => {
            input.reply_failed(e);
            return;
        }
    };
    let (_, ctx) = match resolver.resolve() {
        Ok(resolved) => resolved,
        Err(e) => {
            input.reply_failed(e);
            return;
        }
    };
    let Some(target) = ctx.bevy_entity(eid) else {
        input.reply_failed(format!(
            "entity {} is not live in scene '{}'",
            cmd.entity, ctx.title
        ));
        return;
    };
    let Ok((camera, camera_transform)) = camera.single() else {
        input.reply_failed("no primary camera");
        return;
    };

    let anchor = if cmd.bounds {
        Anchor::Bounds
    } else {
        Anchor::Origin
    };
    let Some(point) = anchor_point(target, anchor, &transforms, &meshes, &children) else {
        input.reply_failed(match anchor {
            Anchor::Bounds => format!(
                "entity {} has no visible mesh with bounds (invisible, or still loading)",
                cmd.entity
            ),
            Anchor::Origin => format!("entity {} has no position yet", cmd.entity),
        });
        return;
    };
    let anchor = match anchor {
        Anchor::Bounds => "bounds",
        Anchor::Origin => "origin",
    };

    let screen = match project(camera, camera_transform, point) {
        Ok(screen) => screen,
        Err(e) => {
            input.reply_failed(e);
            return;
        }
    };
    let json = screen.to_json(&cmd.entity, &ctx.hash, anchor).to_string();

    // The root, the player and the camera are placed by the engine and carry no Transform in the
    // scene's own state; any other entity without one sits at the scene's origin by default, which
    // is no position to click.
    if eid.id <= SceneEntityId::CAMERA.id {
        input.reply_ok(json);
        return;
    }
    let entity = cmd.entity;
    let (tx, rx) = tokio::sync::oneshot::channel();
    match resolver.request_snapshot(&mut pending, move |crdt| {
        let _ = tx.send(if has_transform(crdt, eid) {
            Ok(json)
        } else {
            Err(format!(
                "entity {entity} has no Transform component, so it has no position"
            ))
        });
    }) {
        Ok(()) => console_responses.push_oneshot(rx, |r| r, input.take_responder()),
        Err(e) => input.reply_failed(e),
    }
}

/// Whether the scene has put a Transform on `entity`
fn has_transform(crdt: &CrdtStore, entity: SceneEntityId) -> bool {
    crdt.lww
        .get(&SceneComponentId::TRANSFORM)
        .and_then(|state| state.last_write.get(&entity))
        .is_some_and(|entry| entry.is_some)
}

/// Where a world point lands on the primary camera's view
#[derive(Debug)]
struct ScreenPoint {
    /// logical window pixels from the top left, the space `/click_at` and the pointer ray use;
    /// `None` behind the camera or nearer than its near plane, where there is no such place
    position: Option<Vec2>,
    /// inside the camera's viewport, which is the whole window unless the camera has a smaller one
    on_screen: bool,
    in_front: bool,
    /// distance along the view direction, negative behind the camera
    depth: f32,
    /// straight-line distance from the camera
    distance: f32,
    /// the camera's viewport, in the same logical window pixels
    viewport: Rect,
}

impl ScreenPoint {
    fn to_json(&self, entity: &str, scene: &str, anchor: &str) -> serde_json::Value {
        // hundredths of a pixel or unit, which is finer than anything a click or a scene can tell
        let round = |v: f32| (f64::from(v) * 100.0).round() / 100.0;
        serde_json::json!({
            "entity": entity,
            "scene": scene,
            "anchor": anchor,
            "x": self.position.map(|p| round(p.x)),
            "y": self.position.map(|p| round(p.y)),
            "onScreen": self.on_screen,
            "inFront": self.in_front,
            "depth": round(self.depth),
            "distance": round(self.distance),
            "viewport": {
                "x": round(self.viewport.min.x),
                "y": round(self.viewport.min.y),
                "width": round(self.viewport.width()),
                "height": round(self.viewport.height()),
            },
        })
    }
}

/// Project `world` through the camera with bevy's own conversion, the inverse of the
/// `viewport_to_world` the pointer ray uses, so the result is where a click picks the point. It
/// already includes the camera viewport's offset and the window's scale factor.
fn project(
    camera: &Camera,
    camera_transform: &GlobalTransform,
    world: Vec3,
) -> Result<ScreenPoint, String> {
    let viewport = camera
        .logical_viewport_rect()
        .ok_or("the camera has no viewport yet")?;
    // bevy cameras look down -Z
    let depth = -camera_transform
        .affine()
        .inverse()
        .transform_point3(world)
        .z;
    let in_front = depth > 0.0;
    let position = if in_front {
        match camera.world_to_viewport(camera_transform, world) {
            Ok(position) => Some(position),
            // nearer than the near plane
            Err(ViewportConversionError::PastNearPlane | ViewportConversionError::PastFarPlane) => {
                None
            }
            Err(e) => return Err(format!("could not project the position: {e}")),
        }
    } else {
        None
    };
    Ok(ScreenPoint {
        on_screen: position.is_some_and(|p| viewport.contains(p)),
        position,
        in_front,
        depth,
        distance: world.distance(camera_transform.translation()),
        viewport,
    })
}

#[cfg(test)]
mod tests {
    use bevy::render::camera::{camera_system, ManualTextureViews, Viewport};
    use bevy::window::{
        PrimaryWindow, WindowCreated, WindowResized, WindowResolution, WindowScaleFactorChanged,
    };
    use dcl_component::{transform_and_parent::DclTransformAndParent, DclReader, DclWriter};
    use dcl_types::interface::CrdtType;

    use super::*;

    const EPSILON: f32 = 1e-3;

    /// A camera with the engine's projection (60 degree vertical fov, bevy's default near plane of
    /// 0.1), set up by bevy's own `camera_system` in a window of `physical` pixels at `scale_factor`.
    fn camera_in_window(
        physical: (f32, f32),
        scale_factor: f32,
        viewport: Option<Viewport>,
        transform: Transform,
    ) -> (Camera, GlobalTransform) {
        let mut app = App::new();
        app.add_event::<WindowResized>()
            .add_event::<WindowCreated>()
            .add_event::<WindowScaleFactorChanged>()
            .add_event::<AssetEvent<Image>>()
            .init_resource::<Assets<Image>>()
            .init_resource::<ManualTextureViews>()
            .add_systems(Update, camera_system);
        app.world_mut().spawn((
            Window {
                resolution: WindowResolution::new(physical.0, physical.1)
                    .with_scale_factor_override(scale_factor),
                ..default()
            },
            PrimaryWindow,
        ));
        let camera = app
            .world_mut()
            .spawn((
                Camera {
                    viewport,
                    ..default()
                },
                Projection::Perspective(PerspectiveProjection {
                    fov: std::f32::consts::PI / 3.0,
                    ..default()
                }),
                transform,
                GlobalTransform::from(transform),
            ))
            .id();
        app.update();
        let world = app.world();
        (
            world.get::<Camera>(camera).unwrap().clone(),
            *world.get::<GlobalTransform>(camera).unwrap(),
        )
    }

    fn default_camera() -> (Camera, GlobalTransform) {
        camera_in_window((1280.0, 720.0), 1.0, None, Transform::IDENTITY)
    }

    /// whether the pointer ray through `position` passes through `world`
    fn ray_passes_through(
        camera: &Camera,
        camera_transform: &GlobalTransform,
        position: Vec2,
        world: Vec3,
    ) -> bool {
        let ray = camera
            .viewport_to_world(camera_transform, position)
            .unwrap();
        let along = (world - ray.origin).dot(*ray.direction);
        let closest = ray.origin + *ray.direction * along;
        closest.distance(world) < EPSILON * 10.0
    }

    #[test]
    fn the_command_system_params_do_not_conflict() {
        // bevy panics at schedule build, not compile time, when two of a system's queries conflict
        IntoSystem::into_system(entity_screen_position_cmd).initialize(&mut World::new());
    }

    #[test]
    fn a_point_straight_ahead_is_the_centre_of_the_window() {
        let (camera, transform) = default_camera();
        let screen = project(&camera, &transform, Vec3::new(0.0, 0.0, -10.0)).unwrap();
        let position = screen.position.unwrap();
        assert!(
            position.distance(Vec2::new(640.0, 360.0)) < EPSILON,
            "{position}"
        );
        assert!(screen.on_screen && screen.in_front);
        assert!((screen.depth - 10.0).abs() < EPSILON);
        assert!((screen.distance - 10.0).abs() < EPSILON);
        assert_eq!(
            screen.viewport,
            Rect::from_corners(Vec2::ZERO, Vec2::new(1280.0, 720.0))
        );
    }

    #[test]
    fn the_position_is_where_the_pointer_ray_picks_the_point() {
        let (camera, transform) = default_camera();
        let world = Vec3::new(2.0, 1.0, -10.0);
        let position = project(&camera, &transform, world)
            .unwrap()
            .position
            .unwrap();
        // right of and above the centre, with y growing downwards as the cursor's does
        assert!(position.x > 640.0 && position.y < 360.0, "{position}");
        assert!(ray_passes_through(&camera, &transform, position, world));
    }

    #[test]
    fn a_moved_and_turned_camera_measures_depth_along_its_view() {
        let eye = Vec3::new(5.0, 2.0, 3.0);
        let transform =
            Transform::from_translation(eye).looking_at(Vec3::new(5.0, 2.0, -7.0), Vec3::Y);
        let (camera, transform) = camera_in_window((1280.0, 720.0), 1.0, None, transform);
        let ahead = project(&camera, &transform, Vec3::new(5.0, 2.0, -7.0)).unwrap();
        assert!((ahead.depth - 10.0).abs() < EPSILON);
        assert!(ahead.position.unwrap().distance(Vec2::new(640.0, 360.0)) < EPSILON);

        // off to the side the straight-line distance is longer than the depth
        let world = Vec3::new(8.0, 2.0, -7.0);
        let aside = project(&camera, &transform, world).unwrap();
        assert!((aside.depth - 10.0).abs() < EPSILON);
        assert!((aside.distance - 109.0_f32.sqrt()).abs() < EPSILON);
        assert!(ray_passes_through(
            &camera,
            &transform,
            aside.position.unwrap(),
            world
        ));
    }

    #[test]
    fn positions_are_logical_pixels_on_a_scaled_window() {
        let (camera, transform) =
            camera_in_window((2560.0, 1440.0), 2.0, None, Transform::IDENTITY);
        let screen = project(&camera, &transform, Vec3::new(0.0, 0.0, -10.0)).unwrap();
        assert!(screen.position.unwrap().distance(Vec2::new(640.0, 360.0)) < EPSILON);
        assert_eq!(screen.viewport.max, Vec2::new(1280.0, 720.0));
    }

    #[test]
    fn a_camera_viewport_offsets_positions_and_bounds_on_screen() {
        // an 800x400 viewport whose corner is at (200, 100) of the window
        let viewport = Viewport {
            physical_position: UVec2::new(200, 100),
            physical_size: UVec2::new(800, 400),
            ..default()
        };
        let (camera, transform) =
            camera_in_window((1280.0, 720.0), 1.0, Some(viewport), Transform::IDENTITY);

        let centre = Vec3::new(0.0, 0.0, -10.0);
        let screen = project(&camera, &transform, centre).unwrap();
        let position = screen.position.unwrap();
        assert!(
            position.distance(Vec2::new(600.0, 300.0)) < EPSILON,
            "{position}"
        );
        assert!(screen.on_screen);
        assert_eq!(screen.viewport.min, Vec2::new(200.0, 100.0));
        assert_eq!(screen.viewport.size(), Vec2::new(800.0, 400.0));
        assert!(ray_passes_through(&camera, &transform, position, centre));

        // inside the window, left of the viewport: placed, but not in the camera's view
        let beside = project(&camera, &transform, Vec3::new(-13.86, 0.0, -10.0)).unwrap();
        let position = beside.position.unwrap();
        assert!(position.x > 0.0 && position.x < 200.0, "{position}");
        assert!(!beside.on_screen);
    }

    #[test]
    fn a_point_off_to_the_side_is_placed_but_not_on_screen() {
        let (camera, transform) = default_camera();
        let screen = project(&camera, &transform, Vec3::new(40.0, 0.0, -10.0)).unwrap();
        assert!(screen.position.unwrap().x > 1280.0);
        assert!(screen.in_front && !screen.on_screen);
    }

    #[test]
    fn a_point_behind_the_camera_has_no_position() {
        let (camera, transform) = default_camera();
        let screen = project(&camera, &transform, Vec3::new(0.0, 0.0, 10.0)).unwrap();
        assert_eq!(screen.position, None);
        assert!(!screen.in_front && !screen.on_screen);
        assert!((screen.depth + 10.0).abs() < EPSILON);
        assert!((screen.distance - 10.0).abs() < EPSILON);

        let json = screen.to_json("512", "hash", "origin");
        assert!(json["x"].is_null() && json["y"].is_null());
        assert_eq!(json["inFront"], false);
        assert_eq!(json["onScreen"], false);
    }

    #[test]
    fn a_point_nearer_than_the_near_plane_has_no_position() {
        let (camera, transform) = default_camera();
        let screen = project(&camera, &transform, Vec3::new(0.0, 0.0, -0.01)).unwrap();
        assert_eq!(screen.position, None);
        assert!(screen.in_front && !screen.on_screen);
    }

    #[test]
    fn a_camera_without_a_viewport_is_an_error() {
        let error = project(&Camera::default(), &GlobalTransform::default(), Vec3::ZERO);
        assert_eq!(error.unwrap_err(), "the camera has no viewport yet");
    }

    #[test]
    fn the_reply_names_what_a_test_needs() {
        let (camera, transform) = default_camera();
        let screen = project(&camera, &transform, Vec3::new(0.0, 0.0, -10.0)).unwrap();
        let json = screen.to_json("512", "bafy", "bounds");
        assert_eq!(json["entity"], "512");
        assert_eq!(json["scene"], "bafy");
        assert_eq!(json["anchor"], "bounds");
        assert_eq!(json["x"], 640.0);
        assert_eq!(json["y"], 360.0);
        assert_eq!(json["onScreen"], true);
        assert_eq!(json["inFront"], true);
        assert_eq!(json["depth"], 10.0);
        assert_eq!(json["distance"], 10.0);
        assert_eq!(json["viewport"]["width"], 1280.0);
        assert_eq!(json["viewport"]["height"], 720.0);
    }

    #[test]
    fn an_entity_has_a_transform_once_the_scene_puts_one_on_it() {
        let entity = SceneEntityId::from_proto_u32(512);
        let other = SceneEntityId::from_proto_u32(513);
        let mut bytes = Vec::new();
        DclWriter::new(&mut bytes).write(&DclTransformAndParent::from_bevy_transform_and_parent(
            &Transform::from_xyz(1.0, 2.0, 3.0),
            SceneEntityId::ROOT,
        ));

        let mut crdt = CrdtStore::default();
        assert!(!has_transform(&crdt, entity));
        crdt.force_update(
            SceneComponentId::TRANSFORM,
            CrdtType::LWW_ENT,
            entity,
            Some(&mut DclReader::new(&bytes)),
        );
        assert!(has_transform(&crdt, entity));
        assert!(!has_transform(&crdt, other));

        // a deleted component is no transform
        crdt.force_update(SceneComponentId::TRANSFORM, CrdtType::LWW_ENT, entity, None);
        assert!(!has_transform(&crdt, entity));
    }
}
