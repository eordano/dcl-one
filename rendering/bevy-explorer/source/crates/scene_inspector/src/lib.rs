use bevy::prelude::*;
use common::sets::SceneSets;
use dcl_component::ComponentNameRegistry;
use scene_runner::update_scene::raycast_result::SuperUserRaycastScene;

mod active_scene;
mod asset_commands;
mod manual_registry;
mod mobile_ui;
mod panel;
mod read_commands;
mod screen_position;
pub mod snapshot;
mod write_commands;

pub use active_scene::{ActiveInspectionScene, SceneResolver};
pub use read_commands::parse_entity_id;
pub use snapshot::PendingSnapshotRequests;

pub struct SceneInspectorPlugin;

impl Plugin for SceneInspectorPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ComponentNameRegistry>();
        app.init_resource::<ActiveInspectionScene>();
        app.init_resource::<PendingSnapshotRequests>();
        app.init_resource::<snapshot::PendingEntityAllocations>();

        app.add_plugins(manual_registry::EngineComponentsPlugin);

        app.add_plugins(read_commands::ReadCommandsPlugin);
        app.add_plugins(panel::SceneInspectorPanelPlugin);
        app.add_plugins(write_commands::WriteCommandsPlugin);
        app.add_plugins(asset_commands::AssetCommandsPlugin);
        app.add_plugins(mobile_ui::MobileUiCommandPlugin);

        // after the scene loop so replies are handled the frame they arrive, and before pruning
        // so a reply that raced a scene breaking is still delivered
        app.add_systems(
            Update,
            (
                snapshot::handle_snapshot_events,
                snapshot::handle_entity_allocated_events,
                snapshot::prune_dead_scene_requests,
            )
                .chain()
                .in_set(SceneSets::PostLoop),
        );
        app.add_systems(Update, sync_super_user_raycast_target);
    }
}

// Mirror the *explicitly pinned* inspection scene (/set_scene <hash>) into the raycast target
// resource. Only set when the super-user scene has deliberately chosen a target — None (follow
// player / unpinned) leaves super-user raycasts at their normal behaviour.
fn sync_super_user_raycast_target(
    active: Res<ActiveInspectionScene>,
    mut target: ResMut<SuperUserRaycastScene>,
) {
    if target.0 != active.0 {
        target.0 = active.0;
    }
}
