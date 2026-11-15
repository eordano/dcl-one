use bevy::prelude::*;
use bevy_console::ConsoleCommand;
use common::structs::{CurrentRealm, NoRenderApp, PreviewMode};
use console::{AddDebugSection, DebugInput, DebugInputKind, DebugSection, DoAddConsoleCommand};

use super::{reload_hash, LiveScenes, ParcelReloadTarget};

const MIN_INTERVAL: f64 = 0.1;
const MAX_INTERVAL: f64 = 60.0;
const LOAD_TIMEOUT: f64 = 30.0;

pub(super) struct RapidReloadPlugin;

impl Plugin for RapidReloadPlugin {
    fn build(&self, app: &mut App) {
        if app.world().contains_resource::<NoRenderApp>()
            || !app
                .world()
                .get_resource::<PreviewMode>()
                .is_some_and(|p| p.is_preview)
        {
            return;
        }
        app.init_resource::<RapidReload>()
            .add_preview_console_command::<RapidReloadCommand, _>(command)
            .add_systems(
                Update,
                tick.after(command)
                    .run_if(|reload: Res<RapidReload>| reload.target.is_some()),
            )
            .add_debug_section(DebugSection {
                title: "Rapid Scene Reload",
                query: "/rapid_scene_reload",
                fields: &[
                    ("Status", "status"),
                    ("Target scene", "hash"),
                    ("Reload cycles", "cycles"),
                    ("Seconds until next reload", "remaining"),
                ],
                actions: &[],
                inputs: &[
                    DebugInput {
                        label: "Rapid reload",
                        field: "enabled",
                        command: "/rapid_scene_reload enabled {value}",
                        kind: DebugInputKind::Toggle,
                    },
                    DebugInput {
                        label: "Interval (seconds)",
                        field: "interval",
                        command: "/rapid_scene_reload interval {value}",
                        kind: DebugInputKind::Number {
                            min: MIN_INTERVAL,
                            max: MAX_INTERVAL,
                            step: 0.1,
                        },
                    },
                ],
            });
    }
}

#[derive(Resource)]
struct RapidReload {
    interval: f64,
    cycles: u64,
    target: Option<Target>,
    status: &'static str,
}

impl Default for RapidReload {
    fn default() -> Self {
        Self {
            interval: 3.0,
            cycles: 0,
            target: None,
            status: "Disabled",
        }
    }
}

struct Target {
    hash: String,
    realm: String,
    next: f64,
    awaiting_until: Option<f64>,
}

#[derive(Debug, PartialEq)]
enum Step {
    Wait,
    Reload,
    Stop(&'static str),
}

impl Target {
    fn step(
        &mut self,
        now: f64,
        interval: f64,
        realm: &str,
        parcel: Option<&str>,
        ready: bool,
    ) -> Step {
        if self.realm != realm {
            return Step::Stop("Realm changed");
        }
        if parcel != Some(self.hash.as_str()) {
            return Step::Stop("Left the target scene");
        }
        if let Some(deadline) = self.awaiting_until {
            if now >= deadline {
                return Step::Stop("Target did not finish reloading");
            }
            if ready {
                self.awaiting_until = None;
                self.next = now + interval;
            }
            return Step::Wait;
        }
        if !ready {
            return Step::Stop("Target is no longer running");
        }
        if now < self.next {
            return Step::Wait;
        }
        self.awaiting_until = Some(now + LOAD_TIMEOUT);
        Step::Reload
    }
}

/// Inspect or configure preview-only reload cycling for one ordinary parcel scene.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/rapid_scene_reload")]
struct RapidReloadCommand {
    #[command(subcommand)]
    setting: Option<Setting>,
}
#[derive(clap::Subcommand)]
enum Setting {
    Enabled {
        #[arg(action = clap::ArgAction::Set)]
        value: bool,
    },
    Interval {
        seconds: f64,
    },
}

fn command(
    mut input: ConsoleCommand<RapidReloadCommand>,
    mut state: ResMut<RapidReload>,
    live: Res<LiveScenes>,
    targets: ParcelReloadTarget,
    realm: Res<CurrentRealm>,
    time: Res<Time<Real>>,
) {
    let Some(Ok(cmd)) = input.take() else {
        return;
    };
    let now = time.elapsed_secs_f64();
    match cmd.setting {
        None => {
            let target = state.target.as_ref();
            input.reply_ok(serde_json::json!({
                "enabled":target.is_some(), "interval":state.interval,"cycles":state.cycles,
                "status":state.status, "hash":target.map(|t|&t.hash),
                "remaining":target.filter(|t|t.awaiting_until.is_none()).map(|t| ((t.next-now).max(0.0)*10.0).round()/10.0),
                "unavailable": {
                    "hash": "No active reload target.",
                    "remaining": if target.is_some() { "Waiting for the target scene to restart." } else { "No active reload target." },
                }
            }).to_string());
        }
        Some(Setting::Enabled { value: false }) => {
            state.target = None;
            state.status = "Stopped";
            input.reply_ok("Rapid scene reload stopped");
        }
        Some(Setting::Enabled { value: true }) => {
            if state.target.is_some() {
                input.reply_ok("Rapid scene reload is already running");
                return;
            }
            let Some(hash) = targets.ready_hash(&live).filter(|_| realm.connected) else {
                input.reply_failed("No running parcel scene in a connected realm");
                return;
            };
            state.target = Some(Target {
                hash: hash.to_owned(),
                realm: realm.about_url.clone(),
                next: now + state.interval,
                awaiting_until: None,
            });
            state.cycles = 0;
            state.status = "Waiting for interval";
            input.reply_ok("Rapid scene reload started for this scene");
        }
        Some(Setting::Interval { seconds }) => {
            if !(MIN_INTERVAL..=MAX_INTERVAL).contains(&seconds) {
                input.reply_failed("Interval must be 0.1 to 60 seconds");
                return;
            }
            state.interval = seconds;
            if let Some(t) = &mut state.target {
                t.next = now + seconds;
            }
            input.reply_ok(format!("Reload interval set to {seconds} seconds"));
        }
    }
}

fn tick(
    mut state: ResMut<RapidReload>,
    mut live: ResMut<LiveScenes>,
    targets: ParcelReloadTarget,
    realm: Res<CurrentRealm>,
    preview: Res<PreviewMode>,
    time: Res<Time<Real>>,
) {
    let interval = state.interval;
    let Some(target) = &mut state.target else {
        return;
    };
    let step = if !preview.is_preview {
        Step::Stop("Preview ended")
    } else if !realm.connected {
        Step::Stop("Realm disconnected")
    } else {
        let ready = targets.ready_hash(&live) == Some(target.hash.as_str());
        target.step(
            time.elapsed_secs_f64(),
            interval,
            &realm.about_url,
            targets.parcel_hash(),
            ready,
        )
    };
    match step {
        Step::Reload => {
            if reload_hash(&mut live, &target.hash) {
                state.cycles += 1;
                state.status = "Waiting for scene to restart";
            } else {
                state.target = None;
                state.status = "Target unloaded";
            }
        }
        Step::Wait => {
            state.status = if target.awaiting_until.is_some() {
                "Waiting for scene to restart"
            } else {
                "Waiting for interval"
            }
        }
        Step::Stop(reason) => {
            state.target = None;
            state.status = reason;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target() -> Target {
        Target {
            hash: "scene".into(),
            realm: "realm".into(),
            next: 3.0,
            awaiting_until: None,
        }
    }
    #[test]
    fn restarts_only_the_captured_scene_and_waits_for_its_replacement() {
        let mut t = target();
        assert_eq!(t.step(2.0, 3.0, "realm", Some("scene"), true), Step::Wait);
        assert_eq!(t.step(3.0, 3.0, "realm", Some("scene"), true), Step::Reload);
        assert_eq!(t.step(5.0, 3.0, "realm", Some("scene"), false), Step::Wait);
        assert_eq!(t.step(7.0, 3.0, "realm", Some("scene"), true), Step::Wait);
        assert_eq!(t.next, 10.0);
        assert!(matches!(
            t.step(10.0, 3.0, "realm", Some("other"), true),
            Step::Stop(_)
        ));
    }
    #[test]
    fn times_out_when_a_reload_fails_and_stops_on_unload_or_realm_change() {
        let mut t = target();
        assert!(matches!(
            t.step(1.0, 3.0, "other", Some("scene"), true),
            Step::Stop(_)
        ));
        assert!(matches!(
            t.step(1.0, 3.0, "realm", Some("scene"), false),
            Step::Stop(_)
        ));
        assert_eq!(t.step(3.0, 3.0, "realm", Some("scene"), true), Step::Reload);
        assert!(matches!(
            t.step(33.0, 3.0, "realm", Some("scene"), false),
            Step::Stop(_)
        ));
    }
}
