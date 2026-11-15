//! A windowless explorer: the renderer and the scene, avatar and comms stacks without a
//! window, input or HUD. The avatar renderer (`src/bin/avatar_render.rs`) builds on it; the
//! render-free scene server has its own assembly in `headless`.

use std::time::Duration;

use assets::EmbedAssetsPlugin;
use bevy::{
    app::{ScheduleRunnerPlugin, TaskPoolThreadAssignmentPolicy},
    log::LogPlugin,
    prelude::*,
    window::ExitCondition,
    winit::WinitPlugin,
};
use common::{
    sets::SetupSets,
    structs::{AppConfig, PreviewMode, SceneLoadDistance, TimeOfDay},
    util::UtilsPlugin,
};
use comms::{global_crdt::DiscardPlayerUpdates, CommsPlugin, DisableRealmComms};
use console::ConsolePlugin;
use imposters::DclImposterPlugin;
use ipfs::{map_realm_name, IpfsIoPlugin};
use nft::asset_source::NftReaderPlugin;
use restricted_actions::RestrictedActionsPlugin;
use scene_material::SceneBoundPlugin;
use scene_runner::SceneRunnerPlugin;
use system_bridge::SystemBridgePlugin;
use ui_core::{scrollable::ScrollTargetEvent, UiCorePlugin};
use wallet::Wallet;

use crate::headless::scene_runtime_resources;

/// The app, booted into `realm` (a catalyst root) with content resolved at `content_server`
/// instead of the realm's advertised content server.
pub fn offline_app(config: AppConfig, realm: &str, content_server: String) -> App {
    let mut app = App::new();
    app.add_plugins(
        DefaultPlugins
            .set(TaskPoolPlugin {
                task_pool_options: TaskPoolOptions {
                    io: TaskPoolThreadAssignmentPolicy {
                        min_threads: 4,
                        max_threads: 4,
                        percent: 0.25,
                        on_thread_spawn: None,
                        on_thread_destroy: None,
                    },
                    ..Default::default()
                },
            })
            .set(WindowPlugin {
                primary_window: None,
                exit_condition: ExitCondition::DontExit,
                ..Default::default()
            })
            .set(bevy::asset::AssetPlugin {
                // asset loads go through the ipfs module
                unapproved_path_mode: bevy::asset::UnapprovedPathMode::Allow,
                ..Default::default()
            })
            .set(
                bevy::gltf::GltfPlugin::default()
                    .with_uri_resolver(std::sync::Arc::new(ipfs::ipfs_path::resolve_content_uri)),
            )
            .disable::<WinitPlugin>()
            .set(LogPlugin {
                filter: "wgpu=error,naga=error,bevy_animation=error,avatar=info,ipfs=warn"
                    .to_string(),
                ..default()
            })
            .build()
            .add_before::<bevy::asset::AssetPlugin>(IpfsIoPlugin {
                preview: false,
                starting_realm: Some(map_realm_name(realm)),
                content_server_override: Some(content_server),
                assets_root: Default::default(),
                num_slots: config.max_concurrent_remotes,
            })
            .add_before::<IpfsIoPlugin>(NftReaderPlugin),
    );

    app.add_plugins(EmbedAssetsPlugin);
    app.add_plugins(ScheduleRunnerPlugin::run_loop(Duration::ZERO));

    app.insert_resource(SceneLoadDistance {
        load: config.scene_load_distance,
        unload: config.scene_unload_extra_distance,
        load_imposter: 0.0,
    });
    app.insert_resource(config);

    app.configure_sets(Startup, SetupSets::Init.before(SetupSets::Main));

    // never a participant in the realm's comms: no archipelago, no world room
    app.insert_resource(DisableRealmComms(true));
    app.add_plugins(UtilsPlugin)
        .add_plugins(UiCorePlugin)
        .add_plugins(ConsolePlugin {
            add_bevy_console: false,
        })
        .add_plugins(SceneBoundPlugin)
        .add_plugins(SceneRunnerPlugin)
        .add_plugins(CommsPlugin)
        .add_plugins(RestrictedActionsPlugin)
        .add_plugins(DclImposterPlugin {
            zip_output: None,
            download: false,
        })
        .add_plugins(SystemBridgePlugin { bare: true });

    scene_runtime_resources(&mut app);

    let mut wallet = Wallet::default();
    wallet.finalize_as_guest();
    app.insert_resource(wallet)
        .add_event::<ScrollTargetEvent>()
        .init_resource::<PreviewMode>()
        // noon, in seconds since midnight
        .insert_resource(TimeOfDay {
            time: 12.0 * 3600.0,
        })
        .insert_resource(DiscardPlayerUpdates(true));

    app
}
