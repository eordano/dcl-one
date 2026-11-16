use std::fmt::Debug;

use bevy::{
    color::palettes,
    picking::{
        backend::ray::{RayId, RayMap},
        pointer::{PointerId, PointerLocation},
        PickSet,
    },
    prelude::*,
    render::camera::{NormalizedRenderTarget, RenderTarget},
    text::{FontSmoothing, LineHeight},
    window::PrimaryWindow,
};
use common::structs::PrimaryCamera;
use dcl_component::proto_components::sdk::components::pb_tween::Mode;

use crate::Tween;

const DEFAULT_FONT: TextFont = TextFont {
    font: Handle::Weak(AssetId::Uuid {
        uuid: AssetId::<Font>::DEFAULT_UUID,
    }),
    font_size: 8.,
    line_height: LineHeight::RelativeToFont(1.2),
    font_smoothing: FontSmoothing::AntiAliased,
};

pub struct TweenDebugPlugin;

impl Plugin for TweenDebugPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, setup);

        if app.is_plugin_added::<MeshPickingPlugin>() {
            app.add_systems(
                PreUpdate,
                primary_camera_rays
                    .in_set(PickSet::ProcessInput)
                    .after(RayMap::repopulate),
            );
            app.add_systems(
                PostUpdate,
                update_plate_position.after(TransformSystem::TransformPropagate),
            );

            app.add_observer(tween_picking);
            app.add_observer(tween_out);

            app.add_systems(
                PostUpdate,
                axis_gizmos.after(TransformSystem::TransformPropagate),
            );
        } else {
            error!("MeshPickingPlugin not added to the app. Tween picking systems are disabled.")
        }
    }
}

#[derive(Component)]
struct TweenDebugPlane;

#[derive(Component)]
#[relationship(relationship_target = TweenedEntity)]
struct TweenDebugPlate(Entity);

#[derive(Component)]
#[relationship_target(relationship = TweenDebugPlate )]
struct TweenedEntity(Entity);

fn setup(mut commands: Commands) {
    commands.spawn((
        TweenDebugPlane,
        Node {
            width: Val::Percent(100.),
            height: Val::Percent(100.),
            ..Default::default()
        },
        GlobalZIndex(1_000_000),
    ));
}

/// Gives the mesh picking backend the primary camera's pointer rays while the camera draws to the
/// scene target (below render scale 1): bevy makes rays only for cameras that draw to the
/// pointer's window. The scene target keeps the window's logical size, so the pointer's position
/// on the window is its position in the camera's viewport.
fn primary_camera_rays(
    mut ray_map: ResMut<RayMap>,
    primary_camera: Query<(Entity, &Camera, &GlobalTransform), With<PrimaryCamera>>,
    primary_window: Query<Entity, With<PrimaryWindow>>,
    pointers: Query<(&PointerId, &PointerLocation)>,
) {
    let (Ok((camera_entity, camera, camera_transform)), Ok(window)) =
        (primary_camera.single(), primary_window.single())
    else {
        return;
    };
    if !camera.is_active || !matches!(camera.target, RenderTarget::Image(_)) {
        return;
    }
    for (&pointer_id, pointer_location) in &pointers {
        let Some(location) = pointer_location.location() else {
            continue;
        };
        let on_window = matches!(
            &location.target,
            NormalizedRenderTarget::Window(target) if target.entity() == window
        );
        let in_viewport = camera
            .logical_viewport_rect()
            .is_some_and(|rect| rect.contains(location.position));
        if !on_window || !in_viewport {
            continue;
        }
        if let Ok(ray) = camera.viewport_to_world(camera_transform, location.position) {
            ray_map
                .map
                .insert(RayId::new(camera_entity, pointer_id), ray);
        }
    }
}

fn update_plate_position(
    mut commands: Commands,
    primary_camera: Single<(&Camera, &GlobalTransform), With<PrimaryCamera>>,
    tweens: Query<(&GlobalTransform, &TweenedEntity), With<Tween>>,
    mut tween_plates: Query<(&mut Node, &ComputedNode), With<TweenDebugPlate>>,
) {
    let (camera, camera_global_transform) = primary_camera.into_inner();
    for (global_transform, tweened_entity) in tweens {
        let tween_debug_plate_entity = *tweened_entity.collection();
        let Ok((mut node, computed_node)) = tween_plates.get_mut(tween_debug_plate_entity) else {
            unreachable!("TweenDebugPlate without Node.");
        };
        if let Ok(viewport_position) = camera
            .world_to_viewport_with_depth(camera_global_transform, global_transform.translation())
        {
            node.display = Display::Grid;
            node.left = Val::Px(viewport_position.x - computed_node.content_size.x / 2.);
            node.top = Val::Px(viewport_position.y - computed_node.content_size.y / 2.);
            commands
                .entity(tween_debug_plate_entity)
                .insert(GlobalZIndex(-viewport_position.z as i32));
        } else {
            node.display = Display::None;
        }
    }
}
fn tween_picking(
    mut trigger: Trigger<Pointer<Over>>,
    mut commands: Commands,
    tweens: Query<(&Tween, Has<TweenDebugPlate>)>,
    tween_debug_plane: Single<Entity, With<TweenDebugPlane>>,
) {
    let entity = trigger.target();
    if let Ok((tween, has_tween_debug_plate)) = tweens.get(entity) {
        trigger.propagate(false);

        if has_tween_debug_plate {
            return;
        }

        let root = commands
            .spawn((
                Node {
                    display: Display::Grid,
                    position_type: PositionType::Absolute,
                    column_gap: Val::Px(4.),
                    ..Default::default()
                },
                ChildOf(*tween_debug_plane),
                TweenDebugPlate(entity),
                BackgroundColor(Color::BLACK.with_alpha(0.75)),
            ))
            .id();
        match &tween.0.mode {
            Some(Mode::Move(data)) => {
                plate_head(&mut commands, 1, root, "Move");
                plate_display_row(&mut commands, 2, root, "Duration", &tween.0.duration);
                plate_display_row(
                    &mut commands,
                    3,
                    root,
                    "Easing function",
                    &tween.0.easing_function(),
                );
                plate_display_row(&mut commands, 4, root, "Start", &data.start);
                plate_display_row(&mut commands, 5, root, "End", &data.end);
                plate_display_row(
                    &mut commands,
                    6,
                    root,
                    "Face direction",
                    &data.face_direction,
                );
                plate_display_row(&mut commands, 7, root, "Playing", &tween.0.playing);
                plate_display_row(
                    &mut commands,
                    8,
                    root,
                    "Current time",
                    &tween.0.current_time,
                );
            }
            Some(Mode::Rotate(data)) => {
                plate_head(&mut commands, 1, root, "Rotate");
                plate_display_row(&mut commands, 2, root, "Duration", &tween.0.duration);
                plate_display_row(
                    &mut commands,
                    3,
                    root,
                    "Easing function",
                    &tween.0.easing_function(),
                );
                plate_display_row(&mut commands, 4, root, "Start", &data.start);
                plate_display_row(&mut commands, 5, root, "End", &data.end);
                plate_display_row(&mut commands, 6, root, "Playing", &tween.0.playing);
                plate_display_row(
                    &mut commands,
                    7,
                    root,
                    "Current time",
                    &tween.0.current_time,
                );
            }
            Some(Mode::Scale(data)) => {
                plate_head(&mut commands, 1, root, "Scale");
                plate_display_row(&mut commands, 2, root, "Duration", &tween.0.duration);
                plate_display_row(
                    &mut commands,
                    3,
                    root,
                    "Easing function",
                    &tween.0.easing_function(),
                );
                plate_display_row(&mut commands, 4, root, "Start", &data.start);
                plate_display_row(&mut commands, 5, root, "End", &data.end);
                plate_display_row(&mut commands, 6, root, "Playing", &tween.0.playing);
                plate_display_row(
                    &mut commands,
                    7,
                    root,
                    "Current time",
                    &tween.0.current_time,
                );
            }
            Some(Mode::TextureMove(data)) => {
                plate_head(&mut commands, 1, root, "TextureMove");
                plate_display_row(&mut commands, 2, root, "Duration", &tween.0.duration);
                plate_display_row(
                    &mut commands,
                    3,
                    root,
                    "Easing function",
                    &tween.0.easing_function(),
                );
                plate_display_row(&mut commands, 4, root, "Start", &data.start);
                plate_display_row(&mut commands, 5, root, "End", &data.end);
                plate_display_row(
                    &mut commands,
                    6,
                    root,
                    "MovementType",
                    &data.movement_type(),
                );
                plate_display_row(&mut commands, 7, root, "Playing", &tween.0.playing);
                plate_display_row(
                    &mut commands,
                    8,
                    root,
                    "Current time",
                    &tween.0.current_time,
                );
            }
            Some(Mode::RotateContinuous(data)) => {
                plate_head(&mut commands, 1, root, "Continuous rotation");
                plate_display_row(&mut commands, 2, root, "Duration", &tween.0.duration);
                plate_display_row(
                    &mut commands,
                    3,
                    root,
                    "Easing function",
                    &tween.0.easing_function(),
                );
                plate_display_row(&mut commands, 4, root, "Direction", &data.direction);
                plate_display_row(&mut commands, 5, root, "Speed", &data.speed);
                plate_display_row(&mut commands, 6, root, "Playing", &tween.0.playing);
                plate_display_row(
                    &mut commands,
                    7,
                    root,
                    "Current time",
                    &tween.0.current_time,
                );
            }
            Some(Mode::MoveContinuous(data)) => {
                plate_head(&mut commands, 1, root, "MoveContinuouus");
                plate_display_row(&mut commands, 2, root, "Duration", &tween.0.duration);
                plate_display_row(
                    &mut commands,
                    3,
                    root,
                    "Easing function",
                    &tween.0.easing_function(),
                );
                plate_display_row(&mut commands, 4, root, "Direction", &data.direction);
                plate_display_row(&mut commands, 5, root, "Speed", &data.speed);
                plate_display_row(&mut commands, 6, root, "Playing", &tween.0.playing);
                plate_display_row(
                    &mut commands,
                    7,
                    root,
                    "Current time",
                    &tween.0.current_time,
                );
            }
            Some(Mode::TextureMoveContinuous(data)) => {
                plate_head(&mut commands, 1, root, "TextureMoveContinuouus");
                plate_display_row(&mut commands, 2, root, "Duration", &tween.0.duration);
                plate_display_row(
                    &mut commands,
                    3,
                    root,
                    "Easing function",
                    &tween.0.easing_function(),
                );
                plate_display_row(&mut commands, 4, root, "Direction", &data.direction);
                plate_display_row(&mut commands, 5, root, "Speed", &data.speed);
                plate_display_row(&mut commands, 6, root, "MovementType", &data.movement_type);
                plate_display_row(&mut commands, 7, root, "Playing", &tween.0.playing);
                plate_display_row(
                    &mut commands,
                    8,
                    root,
                    "Current time",
                    &tween.0.current_time,
                );
            }
            _ => {}
        }
    }
}

fn tween_out(
    mut trigger: Trigger<Pointer<Out>>,
    mut commands: Commands,
    tweens: Query<&TweenedEntity>,
) {
    let entity = trigger.target();
    if let Ok(tweened_entity) = tweens.get(entity) {
        trigger.propagate(false);

        commands.entity(*tweened_entity.collection()).despawn();
    };
}

fn plate_head(commands: &mut Commands, row: i16, parent: Entity, text: &str) -> impl Bundle {
    commands.spawn((
        Node {
            grid_row: GridPlacement::start(row),
            grid_column: GridPlacement::start(1),
            ..Default::default()
        },
        Text::new(text),
        DEFAULT_FONT,
        ChildOf(parent),
    ));
}

fn plate_display_row<T: Debug>(
    commands: &mut Commands,
    row: i16,
    parent: Entity,
    text: &str,
    data: &T,
) -> impl Bundle {
    commands.spawn((
        Node {
            grid_row: GridPlacement::start(row),
            grid_column: GridPlacement::start(1),
            ..Default::default()
        },
        Text::new(text),
        DEFAULT_FONT,
        ChildOf(parent),
    ));
    commands.spawn((
        Node {
            grid_row: GridPlacement::start(row),
            grid_column: GridPlacement::start(2),
            ..Default::default()
        },
        Text::new(format!("{:?}", data)),
        DEFAULT_FONT,
        ChildOf(parent),
    ));
}

fn axis_gizmos(mut gizmos: Gizmos, tweens: Query<(&Tween, &GlobalTransform)>) {
    for (tween, global_transform) in tweens {
        match &tween.0.mode {
            Some(Mode::Move(data)) => {
                gizmos.arrow(
                    global_transform.translation(),
                    data.end.unwrap().world_vec_to_vec3(),
                    palettes::tailwind::RED_700,
                );
            }
            Some(Mode::Rotate(data)) => {
                let start = global_transform.rotation();
                let end = data.end.unwrap().to_bevy_normalized();
                let angle = end.angle_between(start);
                gizmos.arc_3d(
                    angle,
                    5.,
                    Isometry3d::from_translation(global_transform.translation()),
                    palettes::tailwind::RED_700,
                );
            }
            Some(Mode::Scale(data)) => {
                let start = global_transform.scale().length().sqrt();
                let end = data.end.unwrap().world_vec_to_vec3().length().sqrt();
                gizmos.sphere(
                    Isometry3d::from_translation(global_transform.translation()),
                    start,
                    palettes::tailwind::RED_300,
                );
                gizmos.sphere(
                    Isometry3d::from_translation(global_transform.translation()),
                    end,
                    palettes::tailwind::RED_700,
                );
            }
            Some(Mode::RotateContinuous(data)) => {
                gizmos.axes(
                    Isometry3d::from_translation(global_transform.translation()),
                    2.5,
                );
                let axis = crate::rotate_continuous_axis(data.direction.unwrap());
                gizmos.arrow(
                    global_transform.translation(),
                    global_transform.translation() + axis * 2.5,
                    palettes::tailwind::RED_700,
                );
            }
            Some(Mode::MoveContinuous(data)) => {
                let direction = data.direction.unwrap().world_vec_to_vec3();
                let speed = data.speed;
                gizmos.arrow(
                    global_transform.translation(),
                    global_transform.translation() + direction * speed,
                    palettes::tailwind::ORANGE_500,
                );
            }
            Some(Mode::TextureMoveContinuous(data)) => {
                let dcl_vec2 = data.direction.unwrap();
                // The direction is in pixels, so we scale it down for the gizmo
                let direction = Vec3::new(dcl_vec2.x, dcl_vec2.y, 0.) / 1024.;
                let speed = data.speed;
                gizmos.arrow(
                    global_transform.translation(),
                    global_transform.translation()
                        + global_transform.rotation() * direction * speed,
                    palettes::tailwind::PURPLE_300,
                );
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use bevy::{
        asset::RenderAssetUsages,
        math::FloatOrd,
        picking::pointer::Location,
        render::{
            camera::{camera_system, ImageRenderTarget, ManualTextureViews},
            render_resource::{Extent3d, TextureDimension, TextureFormat},
        },
        window::{
            WindowCreated, WindowRef, WindowResized, WindowResolution, WindowScaleFactorChanged,
        },
    };

    use super::*;

    /// An 800x600 primary window with the mouse at its centre, and the primary camera drawing to
    /// `target`. Returns the app after two updates and the camera.
    fn rig(target: impl FnOnce(&mut App) -> RenderTarget) -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default()))
            .init_asset::<Image>()
            .init_resource::<ManualTextureViews>()
            .init_resource::<RayMap>()
            .add_event::<WindowResized>()
            .add_event::<WindowCreated>()
            .add_event::<WindowScaleFactorChanged>()
            .add_systems(Update, (camera_system, primary_camera_rays).chain());
        let window = app
            .world_mut()
            .spawn((
                Window {
                    resolution: WindowResolution::new(800.0, 600.0).with_scale_factor_override(1.0),
                    ..default()
                },
                PrimaryWindow,
            ))
            .id();
        let target = target(&mut app);
        let camera = app
            .world_mut()
            .spawn((
                Camera {
                    target,
                    ..default()
                },
                Projection::default(),
                GlobalTransform::default(),
                PrimaryCamera::default(),
            ))
            .id();
        let on_window = RenderTarget::Window(WindowRef::Primary)
            .normalize(Some(window))
            .unwrap();
        assert!(matches!(on_window, NormalizedRenderTarget::Window(_)));
        app.world_mut().spawn((
            PointerId::Mouse,
            PointerLocation::new(Location {
                target: on_window,
                position: Vec2::new(400.0, 300.0),
            }),
        ));
        app.update();
        app.update();
        (app, camera)
    }

    #[test]
    fn a_camera_drawing_to_an_image_gets_the_window_pointers_ray() {
        // half the window's pixels at half its scale factor: the window's logical size
        let (app, camera) = rig(|app| {
            let image = Image::new_fill(
                Extent3d {
                    width: 400,
                    height: 300,
                    depth_or_array_layers: 1,
                },
                TextureDimension::D2,
                &[0; 4],
                TextureFormat::Rgba8UnormSrgb,
                RenderAssetUsages::all(),
            );
            RenderTarget::Image(ImageRenderTarget {
                handle: app.world_mut().resource_mut::<Assets<Image>>().add(image),
                scale_factor: FloatOrd(0.5),
            })
        });
        let ray = app
            .world()
            .resource::<RayMap>()
            .map
            .get(&RayId::new(camera, PointerId::Mouse))
            .expect("a ray for the primary camera");
        assert!(ray.direction.dot(Vec3::NEG_Z) > 0.999, "{ray:?}");
    }

    #[test]
    fn a_camera_drawing_to_the_window_is_left_to_bevy() {
        let (app, _) = rig(|_| RenderTarget::Window(WindowRef::Primary));
        assert!(app.world().resource::<RayMap>().map.is_empty());
    }
}
