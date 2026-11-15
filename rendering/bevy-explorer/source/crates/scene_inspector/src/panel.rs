use bevy::prelude::*;
use bevy_console::ConsoleCommand;
use common::structs::PreviewMode;
use console::{AddDebugSection, DebugInput, DebugInputKind, DebugSection, DoAddConsoleCommand};
use scene_runner::parcel_to_vec3;
use serde_json::json;

use crate::active_scene::SceneResolver;

/// Independent from the editor's active inspection scene and super-user raycast target.
#[derive(Resource, Default)]
pub(crate) struct InspectionParcel {
    parcel: Option<IVec2>,
    preview: bool,
}

impl InspectionParcel {
    pub(super) fn resolve(&self, resolver: &SceneResolver) -> Result<Entity, String> {
        if !self.preview {
            return Err("Entity inspection is only available in preview".into());
        }
        let root = match self.parcel {
            Some(parcel) => resolver
                .containing_scene
                .get_parcel_position(parcel_to_vec3(parcel))
                .ok_or_else(|| format!("no loaded scene at {},{}", parcel.x, parcel.y)),
            None => resolver
                .player
                .single()
                .ok()
                .and_then(|p| resolver.containing_scene.get_parcel(p))
                .ok_or_else(|| "player is not in a loaded parcel scene".to_owned()),
        }?;
        if resolver.super_user.contains(root)
            || resolver
                .scenes
                .get(root)
                .ok()
                .is_none_or(|(_, s)| s.is_portable)
        {
            return Err("Only ordinary loaded parcel scenes can be inspected".into());
        }
        Ok(root)
    }
}

pub(super) struct SceneInspectorPanelPlugin;

impl Plugin for SceneInspectorPanelPlugin {
    fn build(&self, app: &mut App) {
        let preview = app
            .world()
            .get_resource::<PreviewMode>()
            .is_some_and(|p| p.is_preview);
        app.insert_resource(InspectionParcel {
            parcel: None,
            preview,
        });
        if !preview {
            return;
        }
        app.add_preview_console_command::<InspectionState, _>(inspection_state)
            .add_preview_console_command::<InspectParcel, _>(inspect_parcel)
            .add_debug_section(DebugSection {
                title: "World Entities",
                query: "/world_entities_state",
                fields: &[("Inspecting", "mode"), ("Scene", "scene")],
                actions: &[
                    ("Follow player", "/inspect_parcel"),
                    ("List all entities", "/scene_entities --panel"),
                    ("Read recent scene logs", "/scene_logs --panel"),
                ],
                inputs: &[
                    DebugInput {
                        label: "Scene parcel (x,y)",
                        field: "parcel",
                        command: "/inspect_parcel {value}",
                        kind: DebugInputKind::Text,
                    },
                    DebugInput {
                        label: "List components for entity ID",
                        field: "",
                        command: "/entity_components --panel {value}",
                        kind: DebugInputKind::Text,
                    },
                ],
            });
    }
}

#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/world_entities_state")]
struct InspectionState;

fn inspection_state(
    mut input: ConsoleCommand<InspectionState>,
    target: Res<InspectionParcel>,
    resolver: SceneResolver,
) {
    if let Some(Ok(_)) = input.take() {
        let scene = target.resolve(&resolver).and_then(|e| {
            resolver
                .scenes
                .get(e)
                .map(|(_, context)| &context.title)
                .map_err(|_| "The selected parcel scene is no longer loaded.".to_owned())
        });
        input.reply_ok(
            json!({
                "mode": if target.parcel.is_some() { "Selected parcel" } else { "Player's parcel" },
                "parcel": target.parcel.map(|p| format!("{},{}", p.x, p.y)),
                "scene": scene.as_ref().ok(),
                "unavailable": { "scene": scene.as_ref().err() },
            })
            .to_string(),
        );
    }
}

/// Inspect a loaded parcel in the debug panel, or follow the player when no parcel is supplied.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/inspect_parcel")]
struct InspectParcel {
    #[arg(allow_hyphen_values = true)]
    parcel: Option<String>,
}

fn inspect_parcel(mut input: ConsoleCommand<InspectParcel>, mut target: ResMut<InspectionParcel>) {
    if let Some(Ok(cmd)) = input.take() {
        if let Some(text) = cmd.parcel {
            let parsed = text
                .split_once(',')
                .and_then(|(x, y)| Some(IVec2::new(x.parse().ok()?, y.parse().ok()?)));
            let Some(parcel) = parsed else {
                input.reply_failed("expected integer parcel coordinates x,y");
                return;
            };
            target.parcel = Some(parcel);
            input.reply_ok(format!("Inspecting parcel {},{}", parcel.x, parcel.y));
        } else {
            target.parcel = None;
            input.reply_ok("Following player");
        }
    }
}
