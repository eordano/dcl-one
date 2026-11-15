//! The jump marker's debug section and `/jump_marker` console command.

use bevy::prelude::*;
use bevy_console::ConsoleCommand;
use common::structs::{AvatarDynamicState, PrimaryUser};
use console::{AddDebugSection, DebugInput, DebugInputKind, DebugSection, DoAddConsoleCommand};

use super::{marker_opacity, marker_scale, JumpMarker, JumpMarkerEnabled};

pub(super) struct JumpMarkerDiagnosticsPlugin;

impl Plugin for JumpMarkerDiagnosticsPlugin {
    fn build(&self, app: &mut App) {
        app.add_console_command::<JumpMarkerCommand, _>(jump_marker_command)
            .add_debug_section(DebugSection {
                title: "Jump marker",
                query: "/jump_marker",
                fields: &[
                    ("Shown", "shown"),
                    ("Ground distance (m)", "ground_distance"),
                    ("Opacity", "opacity"),
                    ("Blob scale", "scale"),
                ],
                actions: &[],
                inputs: &[DebugInput {
                    label: "Enable jump marker",
                    field: "enabled",
                    command: "/jump_marker {value}",
                    kind: DebugInputKind::Toggle,
                }],
            });
    }
}

/// Read the jump marker's state, or enable or disable it.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/jump_marker")]
struct JumpMarkerCommand {
    enabled: Option<bool>,
}

fn jump_marker_command(
    mut cmd: ConsoleCommand<JumpMarkerCommand>,
    mut enabled: ResMut<JumpMarkerEnabled>,
    player: Query<&AvatarDynamicState, With<PrimaryUser>>,
    marker: Query<&InheritedVisibility, With<JumpMarker>>,
) {
    let Some(Ok(request)) = cmd.take() else {
        return;
    };
    if let Some(value) = request.enabled {
        enabled.0 = value;
    }
    let ground_distance = player.single().ok().map(|state| state.ground_height);
    // effective: false while the player itself is hidden
    let shown = marker.single().is_ok_and(|visibility| visibility.get());
    cmd.reply_ok(
        serde_json::json!({
            "enabled": enabled.0,
            "shown": shown,
            "ground_distance": ground_distance,
            "opacity": ground_distance.map(marker_opacity),
            "scale": ground_distance.map(marker_scale),
            "unavailable": { "": "no primary player" },
        })
        .to_string(),
    );
}
