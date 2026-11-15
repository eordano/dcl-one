//! Console readouts and the runtime debug panel section for smart wearables.

use bevy::prelude::*;
use bevy_console::ConsoleCommand;
use common::structs::{PortableExperiencesToggle, PrimaryUser};
use console::{AddDebugSection, DebugInput, DebugInputKind, DebugSection, DoAddConsoleCommand};
use scene_runner::{
    portables::current_toggle, renderer_context::RendererSceneContext, ContainingScene,
};
use serde_json::json;
use system_bridge::PortableState;

use super::{Model, SmartWearables};
use crate::portables::PortableScenesView;

pub(super) struct SmartWearablesDiagnosticsPlugin;

impl Plugin for SmartWearablesDiagnosticsPlugin {
    fn build(&self, app: &mut App) {
        app.add_console_command::<SmartWearablesCommand, _>(smart_wearables)
            .add_console_command::<SmartWearablesStateCommand, _>(smart_wearables_state)
            .add_debug_section(DebugSection {
                title: "Smart wearables",
                query: "/smart_wearables_state",
                fields: &[
                    (
                        "Current scene allows portable experiences",
                        "currentSceneAllows",
                    ),
                    ("Current scene hides their UI", "currentSceneHidesUi"),
                    ("Body shape", "bodyShape"),
                    ("Realm preview wearables", "previewCount"),
                    ("Entries (urn / state / reason)", "entries"),
                ],
                actions: &[],
                inputs: &[
                    DebugInput {
                        label: "Run without equipping (urn)",
                        field: "",
                        command: "/smart_wearable run {value}",
                        kind: DebugInputKind::Text,
                    },
                    DebugInput {
                        label: "Stop (urn)",
                        field: "",
                        command: "/smart_wearable kill {value}",
                        kind: DebugInputKind::Text,
                    },
                    DebugInput {
                        label: "Restart (urn)",
                        field: "",
                        command: "/smart_wearable restart {value}",
                        kind: DebugInputKind::Text,
                    },
                ],
            });
    }
}

/// List the smart wearables and their states.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/smart_wearables")]
struct SmartWearablesCommand;

/// The smart wearables state as JSON, for the debug panel.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/smart_wearables_state")]
struct SmartWearablesStateCommand;

/// Each entry's urn, state (`loading|running|blocked|stopped|notSmart|failed`) and reason.
pub fn entries(
    model: &Model,
    scene: impl Fn(&str) -> (PortableState, Option<String>),
) -> Vec<(String, String, Option<String>)> {
    model
        .entries
        .keys()
        .map(|urn| match model.state_of(urn, &scene) {
            Some((state, reason)) => {
                let state = serde_json::to_value(state)
                    .ok()
                    .and_then(|v| v.as_str().map(ToOwned::to_owned))
                    .unwrap_or_default();
                (urn.clone(), state, reason)
            }
            None => (urn.clone(), "notSmart".to_owned(), None),
        })
        .collect()
}

/// `current scene allows: <bool>` then `<urn> <state> [<reason>]` per entry.
pub fn format_listing(allows: bool, entries: &[(String, String, Option<String>)]) -> String {
    if entries.is_empty() {
        return "no smart wearables".to_owned();
    }
    let mut lines = vec![format!("current scene allows: {allows}")];
    for (urn, state, reason) in entries.iter() {
        lines.push(match reason {
            Some(reason) => format!("{urn} {state} {reason}"),
            None => format!("{urn} {state}"),
        });
    }
    lines.join("\n")
}

fn toggle(
    player: &Query<Entity, With<PrimaryUser>>,
    containing_scene: &ContainingScene,
    contexts: &Query<&RendererSceneContext>,
) -> PortableExperiencesToggle {
    current_toggle(player.single().ok(), containing_scene, |scene| {
        contexts.get(scene).ok()
    })
}

fn smart_wearables(
    mut input: ConsoleCommand<SmartWearablesCommand>,
    sw: Res<SmartWearables>,
    view: PortableScenesView,
    player: Query<Entity, With<PrimaryUser>>,
    containing_scene: ContainingScene,
    contexts: Query<&RendererSceneContext>,
) {
    let Some(Ok(_)) = input.take() else {
        return;
    };
    let allows =
        toggle(&player, &containing_scene, &contexts) != PortableExperiencesToggle::Disabled;
    let entries = entries(&sw.model, |hash| view.scene_state(hash));
    input.reply_ok(format_listing(allows, &entries));
}

fn smart_wearables_state(
    mut input: ConsoleCommand<SmartWearablesStateCommand>,
    sw: Res<SmartWearables>,
    view: PortableScenesView,
    player: Query<Entity, With<PrimaryUser>>,
    containing_scene: ContainingScene,
    contexts: Query<&RendererSceneContext>,
) {
    let Some(Ok(_)) = input.take() else {
        return;
    };
    let toggle = toggle(&player, &containing_scene, &contexts);
    let entries: Vec<String> = entries(&sw.model, |hash| view.scene_state(hash))
        .into_iter()
        .map(|(urn, state, reason)| match reason {
            Some(reason) => format!("{urn} / {state} / {reason}"),
            None => format!("{urn} / {state}"),
        })
        .collect();
    input.reply_ok(
        json!({
            "currentSceneAllows": toggle != PortableExperiencesToggle::Disabled,
            "currentSceneHidesUi": toggle != PortableExperiencesToggle::Enabled,
            "bodyShape": sw.model.equipped.as_ref().map(|e| e.shape),
            "previewCount": sw.model.preview_count(),
            "entries": if entries.is_empty() { None } else { Some(entries) },
            "unavailable": {
                "bodyShape": "No profile: the player is logged out.",
                "entries": "No smart wearable is equipped, previewed by the realm or run from the console.",
            },
        })
        .to_string(),
    );
}
