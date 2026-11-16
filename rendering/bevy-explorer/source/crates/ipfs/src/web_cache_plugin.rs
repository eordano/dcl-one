//! The web content cache's cap, its trim requests and its debug section (`web_cache`).

use bevy::{prelude::*, tasks::IoTaskPool};
use bevy_console::ConsoleCommand;
use common::{structs::AppConfig, util::TaskCompat};
use console::{AddDebugSection, DebugInput, DebugInputKind, DebugSection, DoAddConsoleCommand};

use crate::web_cache;

pub struct WebCachePlugin {
    pub preview: bool,
}

/// A tester's session override of the cap, in bytes; never saved. Only this engine's: the scene
/// server's frame runs a second engine that still trims to its configured cap, so an override above
/// it lasts until that engine's next trim.
#[derive(Resource, Default)]
struct WebCacheCapOverride(Option<u64>);

const MAX_OVERRIDE_MIB: f64 = 65_536.0;

impl Plugin for WebCachePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<WebCacheCapOverride>()
            .add_systems(
                Update,
                apply_cap.run_if(
                    resource_changed::<AppConfig>.or(resource_changed::<WebCacheCapOverride>),
                ),
            )
            .add_console_command::<WebCacheStateCommand, _>(web_cache_state);
        let inputs: &'static [DebugInput] = if self.preview {
            &[DebugInput {
                label: "Cap override for this session (MiB, 0 = configured)",
                field: "overrideMiB",
                command: "/web_content_cache_cap {value}",
                kind: DebugInputKind::Number {
                    min: 0.0,
                    max: MAX_OVERRIDE_MIB,
                    step: 1.0,
                },
            }]
        } else {
            &[]
        };
        if self.preview {
            app.add_preview_console_command::<WebCacheCapCommand, _>(web_cache_cap);
        }
        app.add_debug_section(DebugSection {
            title: "Web Content Cache",
            query: "/web_content_cache",
            fields: &[
                ("Cap the engine asks for (MiB)", "capMiB"),
                ("Cap override (MiB)", "overrideMiB"),
                ("Written to the cache this session (MiB)", "writtenMiB"),
                ("Writes retried after a quota error", "quotaRetries"),
                ("Trims reported", "trims"),
                (
                    "Trim requests unanswered (no service worker, or timed out)",
                    "unansweredTrims",
                ),
                ("Entries evicted this session", "evictedEntriesTotal"),
                ("Evicted this session (MiB)", "evictedTotalMiB"),
                ("Last trim: cap after the quota bound (MiB)", "workerCapMiB"),
                ("Last trim: origin usage (MiB)", "usageMiB"),
                ("Last trim: origin quota (MiB)", "quotaMiB"),
                ("Last trim: Cache Storage usage (MiB)", "cachesMiB"),
                ("Last trim: entries evicted", "evictedEntries"),
                ("Last trim: evicted (MiB)", "evictedMiB"),
                ("Last trim: content cache size (MiB)", "contentMiB"),
                ("Storage persisted", "persisted"),
            ],
            actions: &[],
            inputs,
        });
    }
}

// `AppConfig` changes with every setting: only a new cap asks for a trim (the first run, at start, too).
fn apply_cap(
    config: Res<AppConfig>,
    cap_override: Res<WebCacheCapOverride>,
    mut applied: Local<Option<u64>>,
) {
    let cap = web_cache::effective_cap(config.cache_bytes, cap_override.0);
    if !web_cache::cap_changed(&mut applied, cap) {
        return;
    }
    web_cache::set_cap(cap);
    IoTaskPool::get()
        .spawn_compat(async {
            web_cache::request_trim(false).await;
        })
        .detach();
}

/// The web content cache's cap, writes, quota retries and the service worker's last trim.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/web_content_cache")]
struct WebCacheStateCommand;

fn web_cache_state(
    mut input: ConsoleCommand<WebCacheStateCommand>,
    cap_override: Res<WebCacheCapOverride>,
) {
    if let Some(Ok(_)) = input.take() {
        input.reply_ok(web_cache::state_json(&web_cache::state(cap_override.0)).to_string());
    }
}

/// Override the web content cache's cap for this session, in MiB; 0 restores the configured cap.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/web_content_cache_cap")]
struct WebCacheCapCommand {
    mib: f64,
}

fn web_cache_cap(
    mut input: ConsoleCommand<WebCacheCapCommand>,
    mut cap_override: ResMut<WebCacheCapOverride>,
) {
    let Some(Ok(cmd)) = input.take() else { return };
    if !(0.0..=MAX_OVERRIDE_MIB).contains(&cmd.mib) {
        input.reply_failed("The cap must be 0 to 65536 MiB");
        return;
    }
    cap_override.0 = web_cache::override_bytes(cmd.mib);
    match cap_override.0 {
        Some(bytes) => input.reply_ok(format!(
            "Web content cache cap override: {} MiB",
            bytes as f64 / (1u64 << 20) as f64
        )),
        None => input.reply_ok("Web content cache cap override removed"),
    }
}
