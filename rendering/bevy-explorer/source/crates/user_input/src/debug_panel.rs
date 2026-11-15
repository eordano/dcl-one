use bevy::prelude::*;
use bevy_console::ConsoleCommand;
use common::structs::{AvatarDynamicState, PrimaryCamera, PrimaryUser};
use console::{AddDebugSection, DebugInput, DebugInputKind, DebugSection, DoAddConsoleCommand};
use dcl_component::proto_components::common::Vector3;
use serde_json::json;

use crate::avatar_movement::{
    ActivePlayerComponent, AvatarLocomotionSettings, AvatarMovement, AvatarMovementInfo,
};

pub(super) struct InputDebugPanelPlugin;

impl Plugin for InputDebugPanelPlugin {
    fn build(&self, app: &mut App) {
        app.add_console_command::<CameraState, _>(camera_state)
            .add_console_command::<MovementState, _>(movement_state)
            .add_preview_console_command::<CameraSensitivity, _>(camera_sensitivity)
            .add_preview_debug_inputs(DebugSection {
                title: "Camera",
                query: "/camera_state",
                fields: &[
                    ("Follow distance (m)", "distance"),
                    ("Far clip (m)", "far"),
                    ("Vertical FOV (degrees)", "fov"),
                    ("Scene override", "override"),
                ],
                actions: &[],
                inputs: &[DebugInput {
                    label: "Look sensitivity",
                    field: "sensitivity",
                    command: "/camera_sensitivity {value}",
                    kind: DebugInputKind::Number {
                        min: 0.01,
                        max: 100.0,
                        step: 0.01,
                    },
                }],
            })
            .add_debug_section(DebugSection {
                title: "Locomotion",
                query: "/movement_state",
                fields: &[
                    ("Walk speed", "walk"),
                    ("Jog speed", "jog"),
                    ("Run speed", "run"),
                    ("Jump height", "jump"),
                    ("Run jump height", "runJump"),
                    ("Double jump height", "doubleJump"),
                    ("Hard landing cooldown", "landing"),
                    ("Requested velocity (X / Y / Z)", "requested"),
                    ("Actual velocity (X / Y / Z)", "actual"),
                    ("External velocity (X / Y / Z)", "external"),
                ],
                inputs: &[],
                actions: &[],
            });
    }
}

/// Read the player's camera.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/camera_state")]
struct CameraState;

/// Set look sensitivity for the player's camera in preview.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/camera_sensitivity")]
struct CameraSensitivity {
    value: f32,
}

fn camera_sensitivity(
    mut cmd: ConsoleCommand<CameraSensitivity>,
    mut camera: Query<&mut PrimaryCamera>,
) {
    let Some(Ok(args)) = cmd.take() else { return };
    if !(0.01..=100.0).contains(&args.value) {
        cmd.reply_failed("sensitivity must be in 0.01..=100");
        return;
    }
    if let Ok(mut camera) = camera.single_mut() {
        camera.sensitivity = args.value;
        cmd.reply_ok(format!("camera sensitivity {}", args.value));
    } else {
        cmd.reply_failed("player camera unavailable");
    }
}

fn camera_state(
    mut cmd: ConsoleCommand<CameraState>,
    camera: Query<(&PrimaryCamera, &Projection)>,
) {
    let Some(Ok(_)) = cmd.take() else { return };
    let Ok((camera, projection)) = camera.single() else {
        cmd.reply_failed("player camera unavailable");
        return;
    };
    let (far, fov) = match projection {
        Projection::Perspective(p) => (Some(p.far), Some(p.fov.to_degrees())),
        _ => (None, None),
    };
    cmd.reply_ok(json!({"sensitivity":camera.sensitivity, "distance":camera.distance, "far":far, "fov":fov, "override":camera.scene_override.is_some()}).to_string());
}

/// Read the active SDK movement settings and last frame of controller feedback.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/movement_state")]
struct MovementState;

fn movement_state(
    mut cmd: ConsoleCommand<MovementState>,
    active: Query<&ActivePlayerComponent<AvatarLocomotionSettings>, With<PrimaryUser>>,
    player: Query<(&ActivePlayerComponent<AvatarMovement>, &AvatarDynamicState), With<PrimaryUser>>,
    info: Res<AvatarMovementInfo>,
) {
    let Some(Ok(_)) = cmd.take() else { return };
    let loc = active.single().ok().map(|active| &active.component.0);
    let player = player.single().ok();
    let vector = |v: Vector3| [v.x, v.y, v.z];
    let requested = info.0.requested_velocity.or_else(|| {
        player.map(|(movement, _)| Vector3::world_vec_from_vec3(&movement.component.velocity))
    });
    // The controller skips the displacement solve when idle or movement is suppressed.
    // It explicitly sets its velocity to zero in that case, leaving SDK feedback absent.
    let actual = info.0.actual_velocity.or_else(|| {
        player
            .filter(|(_, dynamics)| dynamics.velocity == Vec3::ZERO)
            .map(|(_, dynamics)| Vector3::world_vec_from_vec3(&dynamics.velocity))
    });
    let setting_reason = if loc.is_some() {
        "The active locomotion settings do not specify this value."
    } else {
        "No active locomotion settings for the player."
    };
    cmd.reply_ok(json!({
        "walk":loc.and_then(|s| s.walk_speed), "jog":loc.and_then(|s| s.jog_speed), "run":loc.and_then(|s| s.run_speed),
        "jump":loc.and_then(|s| s.jump_height), "runJump":loc.and_then(|s| s.run_jump_height),
        "doubleJump":loc.and_then(|s| s.double_jump_height), "landing":loc.and_then(|s| s.hard_landing_cooldown),
        "requested":requested.map(vector), "actual":actual.map(vector),
        "external":info.0.external_velocity.map(vector),
        "unavailable":{
            "walk":setting_reason, "jog":setting_reason, "run":setting_reason, "jump":setting_reason,
            "runJump":setting_reason, "doubleJump":setting_reason, "landing":setting_reason,
            "requested":"No active player movement controller.",
            "actual":"No completed player movement step.",
            "external":"No SDK force or impulse in the last frame.",
        },
    }).to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::avatar_movement::{AvatarMovement, FromConfig};
    use bevy_console::{ConsoleCommandEntered, PrintConsoleLine};
    use common::structs::AvatarDynamicState;
    use std::sync::{mpsc, Arc};

    #[test]
    fn stationary_controller_reports_its_velocities_when_sdk_feedback_is_omitted() {
        let mut app = App::new();
        app.add_event::<ConsoleCommandEntered>()
            .add_event::<PrintConsoleLine>()
            .init_resource::<AvatarMovementInfo>()
            .add_systems(Update, movement_state);
        app.world_mut().spawn((
            PrimaryUser::default(),
            ActivePlayerComponent::<AvatarMovement>::from_config(&default()),
            AvatarDynamicState::default(),
        ));
        let (tx, rx) = mpsc::channel();
        app.world_mut().send_event(ConsoleCommandEntered {
            command_name: "/movement_state".into(),
            args: vec![],
            responder: Some(Arc::new(move |result| tx.send(result).unwrap())),
        });
        app.update();
        let reply = rx.try_recv().unwrap().unwrap();
        let report: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(report["requested"], json!([0.0, 0.0, 0.0]));
        assert_eq!(report["actual"], json!([0.0, 0.0, 0.0]));
        assert!(report["external"].is_null());
    }
}
