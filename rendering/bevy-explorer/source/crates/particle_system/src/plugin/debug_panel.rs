use super::{ActiveScenes, ParticleEmitter};
use bevy::prelude::*;
use bevy_console::ConsoleCommand;
use console::{AddDebugSection, DebugSection, DoAddConsoleCommand};

pub(super) struct ParticleDebugPanelPlugin;

impl Plugin for ParticleDebugPanelPlugin {
    fn build(&self, app: &mut App) {
        app.add_console_command::<ParticleState, _>(particle_state)
            .add_debug_section(DebugSection {
                title: "SDK particles",
                query: "/particle_state",
                fields: &[
                    ("Live particles in active scenes", "live"),
                    ("Emitters", "emitters"),
                    ("Allocated slots", "capacity"),
                ],
                actions: &[],
                inputs: &[],
            });
    }
}

/// Read live CPU particle rings for emitters in scenes containing the player.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/particle_state")]
struct ParticleState;

fn particle_state(
    mut cmd: ConsoleCommand<ParticleState>,
    active: Res<ActiveScenes>,
    emitters: Query<&ParticleEmitter>,
) {
    if let Some(Ok(_)) = cmd.take() {
        let mut live = 0_u64;
        let mut capacity = 0_u64;
        let mut count = 0;
        for emitter in emitters
            .iter()
            .filter(|emitter| active.contains(&emitter.scene_root))
        {
            count += 1;
            live += u64::from(emitter.ring.alive());
            capacity += u64::from(emitter.capacity);
        }
        cmd.reply_ok(
            serde_json::json!({ "live": live, "capacity": capacity, "emitters": count })
                .to_string(),
        );
    }
}
