use bevy::prelude::*;
use bevy_console::ConsoleCommand;
use common::structs::AppConfig;
use console::{AddDebugSection, DebugInput, DebugInputKind, DebugSection, DoAddConsoleCommand};
use serde_json::json;
use std::sync::atomic::Ordering;

use crate::{IpfsResource, IPFS_CACHED, IPFS_FAILED, IPFS_IN_FLIGHT, IPFS_NON_IPFS, IPFS_SUCCESS};

pub(super) struct WebRequestsDebugPanelPlugin;

impl Plugin for WebRequestsDebugPanelPlugin {
    fn build(&self, app: &mut App) {
        let preview = app.world().resource::<IpfsResource>().is_preview;
        app.add_console_command::<WebState, _>(web_state)
            .add_debug_section(DebugSection {
                title: "Web Requests",
                query: "/content_request_state",
                fields: &[
                    ("IPFS downloads in flight (including queued)", "inFlight"),
                    ("Successful downloads", "success"),
                    ("Cached reads", "cached"),
                    ("Non-IPFS reads", "local"),
                    ("Failed reads", "failed"),
                    #[cfg(not(target_arch = "wasm32"))]
                    ("IPFS connection failures (native)", "connectFailed"),
                    ("Available request slots", "available"),
                    ("Configured download slots", "slots"),
                    ("Downloaded content bytes", "bytes"),
                    ("Mean time to first body byte (ms)", "firstByteMs"),
                    ("Mean small download (ms, ≤10 KiB)", "smallMs"),
                    ("Mean download time (ms/MiB, ≥10 KiB)", "msPerMiB"),
                    ("Download fill rate (bytes/s, ≥10 KiB)", "fillRate"),
                ],
                actions: &[],
                inputs: &[],
            })
            .add_debug_section(DebugSection {
                title: "Entity Requests",
                query: "/content_request_state",
                fields: &[
                    (
                        "Scene definitions per batch (min / mean / max)",
                        "sceneBatch",
                    ),
                    (
                        "Avatar attachments per batch (min / mean / max)",
                        "attachmentBatch",
                    ),
                ],
                actions: &[],
                inputs: &[],
            });
        if preview {
            app.init_resource::<PreviewDelay>()
                .add_preview_console_command::<DelayCommand, _>(request_delay)
                .add_debug_section(DebugSection {
                    title: "Web Requests Delay",
                    query: "/content_request_delay",
                    fields: &[],
                    actions: &[],
                    inputs: &[
                        DebugInput {
                            label: "Use artificial delay",
                            field: "enabled",
                            command: "/content_request_delay enabled {value}",
                            kind: DebugInputKind::Toggle,
                        },
                        DebugInput {
                            label: "Delay before each IPFS request (seconds)",
                            field: "seconds",
                            command: "/content_request_delay seconds {value}",
                            kind: DebugInputKind::Number {
                                min: 0.0,
                                max: MAX_DELAY_SECONDS,
                                step: 0.1,
                            },
                        },
                    ],
                });
        }
    }
}

const MAX_DELAY_SECONDS: f64 = 10.0;

#[derive(Resource)]
struct PreviewDelay {
    enabled: bool,
    millis: u32,
}
impl Default for PreviewDelay {
    fn default() -> Self {
        Self {
            enabled: false,
            millis: 1_000,
        }
    }
}

/// Inspect or set artificial delay on real preview content requests.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/content_request_delay")]
struct DelayCommand {
    #[command(subcommand)]
    setting: Option<DelaySetting>,
}
#[derive(clap::Subcommand)]
enum DelaySetting {
    Enabled {
        #[arg(action=clap::ArgAction::Set)]
        value: bool,
    },
    Seconds {
        value: f64,
    },
}

fn request_delay(
    mut input: ConsoleCommand<DelayCommand>,
    mut delay: ResMut<PreviewDelay>,
    ipfs: Res<IpfsResource>,
) {
    let Some(Ok(cmd)) = input.take() else { return };
    match cmd.setting {
        None => {
            input.reply_ok(
                json!({"enabled":delay.enabled,"seconds":f64::from(delay.millis)/1000.0})
                    .to_string(),
            );
            return;
        }
        Some(DelaySetting::Enabled { value }) => delay.enabled = value,
        Some(DelaySetting::Seconds { value }) => {
            if !(0.0..=MAX_DELAY_SECONDS).contains(&value) {
                input.reply_failed("Delay must be 0 to 10 seconds");
                return;
            }
            delay.millis = (value * 1000.0).round() as u32;
        }
    }
    let ms = if delay.enabled { delay.millis } else { 0 };
    ipfs.preview_delay_ms.store(ms, Ordering::Relaxed);
    input.reply_ok(format!("Preview content request delay is {ms} ms"));
}

#[cfg(test)]
mod delay_tests {
    use super::*;
    use crate::IpfsIo;
    use bevy::asset::io::memory::MemoryAssetReader;
    use std::sync::Arc;

    fn app(preview: bool) -> App {
        let ipfs = IpfsIo::new(
            preview,
            Box::new(MemoryAssetReader { root: default() }),
            None,
            default(),
            4,
            #[cfg(feature = "ipfs_debug")]
            tokio::sync::mpsc::unbounded_channel().0,
        );
        let mut app = App::new();
        app.insert_resource(IpfsResource {
            inner: Arc::new(ipfs),
        })
        .add_plugins(WebRequestsDebugPanelPlugin);
        app
    }

    #[test]
    fn only_preview_installs_delay_controls_and_starts_disabled() {
        assert!(!app(false).world().contains_resource::<PreviewDelay>());
        assert!(!app(true).world().resource::<PreviewDelay>().enabled);
    }
}

#[cfg(test)]
mod request_tests {
    use super::*;
    use crate::IpfsIo;
    use bevy::asset::io::memory::MemoryAssetReader;
    use bevy_console::{ConsoleCommandEntered, PrintConsoleLine};
    use std::{sync::Arc, time::Duration};

    #[test]
    fn first_content_request_state_includes_completed_downloads_and_batches() {
        let ipfs = Arc::new(IpfsIo::new(
            false,
            Box::new(MemoryAssetReader { root: default() }),
            None,
            default(),
            4,
            #[cfg(feature = "ipfs_debug")]
            tokio::sync::mpsc::unbounded_channel().0,
        ));
        ipfs.record_scene_batch(0);
        ipfs.record_scene_batch(2);
        ipfs.record_attachment_batch(4);
        {
            let mut metrics = ipfs.request_metrics.downloads.lock().unwrap();
            metrics.download(0, Duration::from_millis(500), None);
            metrics.download(
                10 * 1024,
                Duration::from_secs(1),
                Some(Duration::from_millis(50)),
            );
            metrics.download(
                1024 * 1024,
                Duration::from_secs(2),
                Some(Duration::from_millis(150)),
            );
        }

        let mut app = App::new();
        app.add_event::<ConsoleCommandEntered>()
            .add_event::<PrintConsoleLine>()
            .init_resource::<AppConfig>()
            .insert_resource(IpfsResource { inner: ipfs })
            .add_systems(Update, web_state);
        let (tx, rx) = std::sync::mpsc::channel();
        app.world_mut().send_event(ConsoleCommandEntered {
            command_name: "/content_request_state".into(),
            args: vec![],
            responder: Some(Arc::new(move |result| tx.send(result).unwrap())),
        });
        app.update();
        let state: serde_json::Value =
            serde_json::from_str(&rx.try_recv().unwrap().unwrap()).unwrap();
        assert_eq!(state["bytes"], 1_058_816);
        assert_eq!(state["firstByteMs"], 100.0);
        assert_eq!(state["smallMs"], 750.0);
        assert_eq!(state["msPerMiB"], 52_200.0);
        assert_eq!(state["fillRate"], 352_939.0);
        assert_eq!(state["sceneBatch"], json!([0, 1.0, 2]));
        assert_eq!(state["attachmentBatch"], json!([4, 4.0, 4]));
    }
}

/// Content download counters and completed active-entity batch sizes.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/content_request_state")]
struct WebState;

fn web_state(mut input: ConsoleCommand<WebState>, ipfs: Res<IpfsResource>, config: Res<AppConfig>) {
    if let Some(Ok(_)) = input.take() {
        let stats = ipfs.download_stats();
        #[cfg(not(target_arch = "wasm32"))]
        let connect_failed = Some(stats.connect_failed);
        #[cfg(target_arch = "wasm32")]
        let connect_failed: Option<u64> = None;
        input.reply_ok(
            json!({
                "inFlight": IPFS_IN_FLIGHT.load(Ordering::Relaxed),
                "success": IPFS_SUCCESS.load(Ordering::Relaxed),
                "cached": IPFS_CACHED.load(Ordering::Relaxed),
                "local": IPFS_NON_IPFS.load(Ordering::Relaxed),
                "failed": IPFS_FAILED.load(Ordering::Relaxed),
                "connectFailed": connect_failed,
                "available": ipfs.request_slots.available_permits(),
                "slots": config.max_concurrent_remotes,
                "bytes": stats.bytes,
                "firstByteMs": stats.mean_first_byte_ms().map(f64::round),
                "smallMs": stats.mean_small_ms().map(f64::round),
                "msPerMiB": stats.mean_ms_per_mib().map(f64::round),
                "fillRate": stats.bytes_per_second().map(f64::round),
                "sceneBatch": stats.scene_batches.range(),
                "attachmentBatch": stats.attachment_batches.range(),
                "unavailable": {
                    "firstByteMs": "No content download has returned a body byte yet.",
                    "smallMs": "No content download of 10 KiB or less has completed.",
                    "msPerMiB": "No content download of 10 KiB or more has completed.",
                    "fillRate": "No transfer time recorded for downloads of 10 KiB or more.",
                    "sceneBatch": "No scene definition lookup has completed.",
                    "attachmentBatch": "No avatar attachment lookup has completed.",
                },
            })
            .to_string(),
        );
    }
}
