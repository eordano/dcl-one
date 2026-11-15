use bevy::prelude::*;
use bevy_console::ConsoleCommand;
use console::DoAddConsoleCommand;
use scene_runner::MobileUiEmulation;

pub struct MobileUiCommandPlugin;

impl Plugin for MobileUiCommandPlugin {
    fn build(&self, app: &mut App) {
        app.add_console_command::<MobileUiCommand, _>(mobile_ui_cmd);
    }
}

/// Tell scenes they run on a phone: the mobile platform, and the mobile client's interactable area
/// scaled from a 1600-wide canvas to the window. The window keeps its size: on web the editor
/// host makes the canvas phone-shaped. The platform is fixed when a scene spawns (and the SDK
/// caches it), so reload the scene to pick it up; the canvas info applies live. Native touch
/// controls are not drawn.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/mobile_ui")]
struct MobileUiCommand {
    /// on or off (omit to read the current state)
    state: Option<String>,
}

fn mobile_ui_cmd(
    mut input: ConsoleCommand<MobileUiCommand>,
    mut mobile: ResMut<MobileUiEmulation>,
) {
    let Some(Ok(cmd)) = input.take() else {
        return;
    };
    // set_if_neq: switching to the current state must not rebuild the scene UI roots
    match cmd.state.as_deref() {
        None => {}
        Some("on") => {
            mobile.set_if_neq(MobileUiEmulation(true));
        }
        Some("off") => {
            mobile.set_if_neq(MobileUiEmulation(false));
        }
        Some(other) => {
            input.reply_failed(format!("expected on or off, got '{other}'"));
            return;
        }
    }
    input.reply_ok(if mobile.0 { "on" } else { "off" });
}
