//! Run control for the benchmark autopilot (`--autopilot` / `?autopilot`): moves
//! [`AutopilotPhase`] along the run and ends it. `common::autopilot` has the contract; the CSV
//! writer and the log layer follow the phase and read nothing from here, and the writer reports a
//! CSV it could not write with [`CsvFailed`].
//!
//! Signing in is the HUD's (react-web `useEngineSession`, native and web alike): always the local
//! guest, an account of its own whose root key is kept (natively in the config, behind
//! `/login_local_guest`, which only the autopilot may call; on the web by the shell), signed anew
//! every run. A stored login is never used. The engine only learns the outcome: it sees the
//! wallet, and the HUD reports a failed sign-in with `/autopilot_fail`. A wallet that is not a
//! guest account ends the run here too, so a HUD that signed in otherwise fails the run instead of
//! letting scenes, whose fetches are allowed without a prompt, act as a person's account.
//!
//! A run is a measurement only while it stays in world and its CSV is whole: leaving the world or
//! a CSV that fails while measuring ends it with exit code 1.

use bevy::{app::AppExit, diagnostic::FrameCount, prelude::*, time::Real};
use bevy_console::ConsoleCommand;
use chrono::SecondsFormat;
use common::{
    autopilot::{
        Autopilot, AutopilotPhase, CsvFailed, ARRIVE_TIMEOUT_SECONDS, EXIT_MARKER, LOG_TARGET,
        MEASURE_SECONDS,
    },
    structs::{CurrentRealm, OutOfWorld, PrimaryUser, WorldHold},
};
use console::DoAddConsoleCommand;
use wallet::Wallet;

use crate::world_state::{Arrivals, WorldState};

pub(crate) mod walk;

/// Added only when [`Autopilot`] exists. It reads the player's [`Arrivals`], so the app needs
/// `WorldStatePlugin`, which the client adds in every run.
pub struct AutopilotPlugin;

impl Plugin for AutopilotPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(RunControlPlugin)
            .add_console_command::<AutopilotFailCommand, _>(autopilot_fail);
        app.add_plugins(walk::AutopilotWalkPlugin);
    }
}

/// The phase machine, apart from the console command so a test can build it alone.
struct RunControlPlugin;

impl Plugin for RunControlPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Run>()
            // the frame after `Ending`, before `Update`, where the CEF shutdown waits for the exit
            .add_systems(PreUpdate, end_run)
            .add_systems(Update, (log_identity, drive_phase, log_phase).chain());
    }
}

#[derive(Resource, Default)]
struct Run {
    /// `Time<Real>` seconds at which `Measuring` began.
    measuring_since: Option<f64>,
    /// The frame in which `Ending` was first seen here, so the exit comes in a later one.
    ending_frame: Option<u32>,
    identity_logged: bool,
    exit_requested: bool,
}

#[allow(clippy::too_many_arguments)]
fn drive_phase(
    autopilot: Res<Autopilot>,
    mut phase: ResMut<AutopilotPhase>,
    mut run: ResMut<Run>,
    time: Res<Time<Real>>,
    frame: Res<FrameCount>,
    wallet: Res<Wallet>,
    world_hold: Option<Res<WorldHold>>,
    realm: Res<CurrentRealm>,
    csv_failed: Option<Res<CsvFailed>>,
    arrivals: Res<Arrivals>,
    player: Query<Has<OutOfWorld>, With<PrimaryUser>>,
) {
    let now = time.elapsed_secs_f64();
    let player_out_of_world = player.single().ok();
    let signed_in = wallet.address().is_some();
    // the player has to have arrived: it spawns out of world, and arriving is leaving that state,
    // never the lack of it
    let in_world_now = arrivals.0 > 0
        && WorldState {
            signed_in,
            world_held: world_hold.is_some(),
            realm_connected: realm.connected,
            out_of_world: player_out_of_world,
        }
        .in_world();
    let next = match *phase {
        AutopilotPhase::Ending { .. } => None,
        // the local guest is a guest account; scenes fetch without a prompt, so a person's
        // account must never stand in for it
        _ if signed_in && !wallet.is_guest_account() => {
            error!(
                target: LOG_TARGET,
                "signed in as an account that is not a guest account: a run signs in only as the local guest"
            );
            Some(AutopilotPhase::Ending { exit_code: 1 })
        }
        AutopilotPhase::Arriving => {
            if in_world_now {
                if autopilot.measure {
                    run.measuring_since = Some(now);
                    Some(AutopilotPhase::Measuring)
                } else {
                    Some(AutopilotPhase::Ending { exit_code: 0 })
                }
            } else if now >= ARRIVE_TIMEOUT_SECONDS {
                error!(
                    target: LOG_TARGET,
                    "not in world {ARRIVE_TIMEOUT_SECONDS} s after launch (signed in: {signed_in}, world held: {}, realm connected: {})",
                    world_hold.is_some(),
                    realm.connected,
                );
                Some(AutopilotPhase::Ending { exit_code: 1 })
            } else {
                None
            }
        }
        // the writer logged why
        AutopilotPhase::Measuring if csv_failed.is_some() => {
            Some(AutopilotPhase::Ending { exit_code: 1 })
        }
        // rows of an emptied world would read as a faster build
        AutopilotPhase::Measuring if !in_world_now => {
            error!(
                target: LOG_TARGET,
                "left the world while measuring (signed in: {signed_in}, world held: {}, realm connected: {}, player out of world: {})",
                world_hold.is_some(),
                realm.connected,
                match player_out_of_world {
                    Some(true) => "yes",
                    Some(false) => "no",
                    None => "no player",
                },
            );
            Some(AutopilotPhase::Ending { exit_code: 1 })
        }
        AutopilotPhase::Measuring => {
            let since = *run.measuring_since.get_or_insert(now);
            (now - since >= MEASURE_SECONDS).then_some(AutopilotPhase::Ending { exit_code: 0 })
        }
    };
    if let Some(next) = next {
        *phase = next;
    }
    if matches!(*phase, AutopilotPhase::Ending { .. }) && run.ending_frame.is_none() {
        run.ending_frame = Some(frame.0);
    }
}

/// In a frame after `Ending` was set: the marker the harness stops counting exceptions at, then
/// the exit. A CSV whose last flush failed, in the `Ending` frame itself, makes the exit code 1
/// although the time was up.
fn end_run(
    phase: Res<AutopilotPhase>,
    csv_failed: Option<Res<CsvFailed>>,
    mut run: ResMut<Run>,
    frame: Res<FrameCount>,
    mut exit: EventWriter<AppExit>,
) {
    let AutopilotPhase::Ending { exit_code } = *phase else {
        return;
    };
    if run.exit_requested || run.ending_frame.is_none_or(|ended| frame.0 <= ended) {
        return;
    }
    run.exit_requested = true;
    let exit_code = if csv_failed.is_some() {
        exit_code.max(1)
    } else {
        exit_code
    };
    info!(target: LOG_TARGET, "{EXIT_MARKER} at {}", utc_now());
    exit.write(AppExit::from_code(exit_code));
}

fn utc_now() -> String {
    chrono::DateTime::from_timestamp_millis(perf_output::layer::unix_milliseconds() as i64)
        .map(|now| now.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_default()
}

fn log_phase(phase: Res<AutopilotPhase>, time: Res<Time<Real>>) {
    if phase.is_changed() {
        info!(target: LOG_TARGET, "phase {:?} at {:.1} s", *phase, time.elapsed_secs_f64());
    }
}

/// Which identity the run signs in as: the same address in every run proves the guest is the
/// same account.
fn log_identity(wallet: Res<Wallet>, mut run: ResMut<Run>) {
    if run.identity_logged {
        return;
    }
    let Some(address) = wallet.address() else {
        return;
    };
    run.identity_logged = true;
    let kind = if wallet.is_guest_account() {
        "guest account"
    } else if wallet.is_guest() {
        "throwaway guest"
    } else {
        "account"
    };
    info!(target: LOG_TARGET, "signed in as {address:#x} ({kind})");
}

/// The HUD's report that the unattended sign-in failed (no usable identity, a guest that could not
/// be created, a login the engine refused): ends the run with exit code 1.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/autopilot_fail")]
struct AutopilotFailCommand {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    reason: Vec<String>,
}

fn autopilot_fail(
    mut input: ConsoleCommand<AutopilotFailCommand>,
    mut phase: ResMut<AutopilotPhase>,
) {
    let Some(Ok(command)) = input.take() else {
        return;
    };
    if *phase != AutopilotPhase::Arriving {
        input.reply_failed(format!("the run is already {:?}", *phase));
        return;
    }
    let reason = command
        .reason
        .join(" ")
        .replace(['\r', '\n'], " ")
        .chars()
        .take(200)
        .collect::<String>();
    error!(target: LOG_TARGET, "sign-in failed: {reason}");
    *phase = AutopilotPhase::Ending { exit_code: 1 };
    input.reply_ok("run ended");
}

// These tests sign their fixture accounts with the native-only local guest wallet.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::time::Duration;

    use bevy::{
        diagnostic::FrameCountPlugin,
        time::{TimePlugin, TimeUpdateStrategy},
    };

    use super::*;
    use crate::world_state::ArrivalsPlugin;

    /// A minimal app on a manual clock: every `update` advances `Time<Real>` by `step`.
    fn app(measure: bool, step: Duration) -> App {
        let mut app = App::new();
        app.add_plugins((
            TimePlugin,
            FrameCountPlugin,
            ArrivalsPlugin,
            RunControlPlugin,
        ))
        .insert_resource(TimeUpdateStrategy::ManualDuration(step))
        .insert_resource(Autopilot { measure })
        .init_resource::<AutopilotPhase>()
        .init_resource::<Wallet>()
        .init_resource::<CurrentRealm>();
        app
    }

    /// Signs in with an identity a new root key signed: as a guest account, the local guest's
    /// kind, or as a person's account.
    fn sign_in_as(app: &mut App, guest_account: bool) {
        let root = wallet::local_guest::new_root_key();
        let (ephemeral, auth) = wallet::local_guest::sign_identity(&root, 0).unwrap();
        app.world_mut().resource_mut::<Wallet>().finalize(
            root.address(),
            ephemeral,
            auth,
            guest_account,
        );
    }

    fn sign_in(app: &mut App) {
        sign_in_as(app, true);
    }

    fn connect_realm(app: &mut App) {
        app.world_mut().resource_mut::<CurrentRealm>().connected = true;
    }

    fn phase(app: &App) -> AutopilotPhase {
        *app.world().resource::<AutopilotPhase>()
    }

    fn exits(app: &mut App) -> Vec<AppExit> {
        app.world_mut()
            .resource_mut::<Events<AppExit>>()
            .drain()
            .collect()
    }

    /// A player entering the world: `OutOfWorld` until the scene is ready.
    fn spawn_player(app: &mut App) -> Entity {
        app.world_mut()
            .spawn((PrimaryUser::default(), OutOfWorld))
            .id()
    }

    fn arrive(app: &mut App, player: Entity) {
        sign_in(app);
        connect_realm(app);
        app.world_mut().entity_mut(player).remove::<OutOfWorld>();
    }

    #[test]
    fn measures_ninety_seconds_after_arriving_and_exits_in_a_later_frame() {
        let mut app = app(true, Duration::from_secs(1));
        let player = spawn_player(&mut app);
        for _ in 0..10 {
            app.update();
        }
        assert_eq!(phase(&app), AutopilotPhase::Arriving);

        arrive(&mut app, player);
        app.update();
        assert_eq!(phase(&app), AutopilotPhase::Measuring);

        for _ in 0..(MEASURE_SECONDS as u32 - 1) {
            app.update();
        }
        assert_eq!(phase(&app), AutopilotPhase::Measuring);

        // the frame the time is up sets `Ending`; the exit follows in the next one
        app.update();
        assert_eq!(phase(&app), AutopilotPhase::Ending { exit_code: 0 });
        assert!(exits(&mut app).is_empty());
        app.update();
        assert_eq!(exits(&mut app), [AppExit::Success]);

        // once
        app.update();
        assert!(exits(&mut app).is_empty());
    }

    #[test]
    fn a_warm_up_ends_on_arriving() {
        let mut app = app(false, Duration::from_secs(1));
        let player = spawn_player(&mut app);
        app.update();
        arrive(&mut app, player);
        app.update();
        assert_eq!(phase(&app), AutopilotPhase::Ending { exit_code: 0 });
        app.update();
        assert_eq!(exits(&mut app), [AppExit::Success]);
    }

    #[test]
    fn arriving_waits_for_each_part_of_being_in_world() {
        let mut app = app(true, Duration::from_secs(1));
        let player = spawn_player(&mut app);
        let still_arriving = |app: &mut App| {
            app.update();
            assert_eq!(phase(app), AutopilotPhase::Arriving);
        };

        // out of world
        sign_in(&mut app);
        connect_realm(&mut app);
        still_arriving(&mut app);

        // held for a lobby
        app.world_mut().entity_mut(player).remove::<OutOfWorld>();
        app.insert_resource(WorldHold);
        still_arriving(&mut app);
        app.world_mut().remove_resource::<WorldHold>();

        // the realm not connected
        app.world_mut().resource_mut::<CurrentRealm>().connected = false;
        still_arriving(&mut app);
        connect_realm(&mut app);

        // not signed in
        app.world_mut().resource_mut::<Wallet>().disconnect();
        still_arriving(&mut app);

        sign_in(&mut app);
        app.update();
        assert_eq!(phase(&app), AutopilotPhase::Measuring);
    }

    #[test]
    fn a_player_never_seen_out_of_world_has_not_arrived() {
        let mut app = app(true, Duration::from_secs(1));
        // spawned without `OutOfWorld`: nothing was ever loading
        let player = app.world_mut().spawn(PrimaryUser::default()).id();
        sign_in(&mut app);
        connect_realm(&mut app);
        for _ in 0..3 {
            app.update();
        }
        assert_eq!(phase(&app), AutopilotPhase::Arriving);

        // placed out of world for a frame, and then in
        app.world_mut().entity_mut(player).insert(OutOfWorld);
        app.update();
        assert_eq!(phase(&app), AutopilotPhase::Arriving);
        app.world_mut().entity_mut(player).remove::<OutOfWorld>();
        app.update();
        assert_eq!(phase(&app), AutopilotPhase::Measuring);
    }

    #[test]
    fn a_period_out_of_world_that_began_and_ended_between_two_frames_still_counts() {
        let mut app = app(true, Duration::from_secs(1));
        let player = app.world_mut().spawn(PrimaryUser::default()).id();
        sign_in(&mut app);
        connect_realm(&mut app);
        app.update();
        assert_eq!(phase(&app), AutopilotPhase::Arriving);

        // placed out of world and released before any frame sampled it: only the removal is left
        app.world_mut().entity_mut(player).insert(OutOfWorld);
        app.world_mut().entity_mut(player).remove::<OutOfWorld>();
        app.update();
        assert_eq!(phase(&app), AutopilotPhase::Measuring);
    }

    #[test]
    fn another_entity_losing_out_of_world_is_not_the_player_arriving() {
        let mut app = app(true, Duration::from_secs(1));
        app.world_mut().spawn(PrimaryUser::default());
        let other = app.world_mut().spawn(OutOfWorld).id();
        sign_in(&mut app);
        connect_realm(&mut app);
        app.world_mut().entity_mut(other).remove::<OutOfWorld>();
        for _ in 0..3 {
            app.update();
        }
        assert_eq!(phase(&app), AutopilotPhase::Arriving);
    }

    #[test]
    fn a_run_that_never_arrives_fails_after_the_arrive_timeout() {
        // the first update starts the clock
        let mut app = app(true, Duration::from_secs(150));
        spawn_player(&mut app);
        app.update();
        app.update();
        assert_eq!(phase(&app), AutopilotPhase::Arriving);

        app.update();
        let expected = AutopilotPhase::Ending { exit_code: 1 };
        assert_eq!(phase(&app), expected);
        assert!(exits(&mut app).is_empty());
        app.update();
        assert_eq!(exits(&mut app), [AppExit::from_code(1)]);
    }

    #[test]
    fn a_failure_reported_from_outside_ends_the_run() {
        let mut app = app(true, Duration::from_secs(1));
        spawn_player(&mut app);
        app.update();
        *app.world_mut().resource_mut::<AutopilotPhase>() = AutopilotPhase::Ending { exit_code: 1 };
        app.update();
        assert!(exits(&mut app).is_empty());
        app.update();
        assert_eq!(exits(&mut app), [AppExit::from_code(1)]);
    }

    /// Ends with `exit_code` in this update, and exits with it in the next one.
    fn ends_with(app: &mut App, exit_code: u8) {
        app.update();
        assert_eq!(phase(app), AutopilotPhase::Ending { exit_code });
        assert!(exits(app).is_empty());
        app.update();
        assert_eq!(exits(app), [AppExit::from_code(exit_code)]);
    }

    /// A run on the manual clock, in world and measuring.
    fn measuring() -> (App, Entity) {
        let mut app = app(true, Duration::from_secs(1));
        let player = spawn_player(&mut app);
        app.update();
        arrive(&mut app, player);
        app.update();
        assert_eq!(phase(&app), AutopilotPhase::Measuring);
        (app, player)
    }

    #[test]
    fn an_account_that_is_not_a_guest_account_ends_the_run() {
        let mut app = app(true, Duration::from_secs(1));
        spawn_player(&mut app);
        app.update();
        sign_in_as(&mut app, false);
        ends_with(&mut app, 1);
    }

    #[test]
    fn a_throwaway_guest_is_not_the_local_guest() {
        let mut app = app(true, Duration::from_secs(1));
        spawn_player(&mut app);
        app.update();
        app.world_mut().resource_mut::<Wallet>().finalize_as_guest();
        ends_with(&mut app, 1);
    }

    #[test]
    fn an_account_signed_in_while_measuring_ends_the_run() {
        let (mut app, _) = measuring();
        sign_in_as(&mut app, false);
        ends_with(&mut app, 1);
    }

    #[test]
    fn leaving_the_world_while_measuring_ends_the_run() {
        let (mut app, player) = measuring();
        app.update();
        assert_eq!(phase(&app), AutopilotPhase::Measuring);
        app.world_mut().entity_mut(player).insert(OutOfWorld);
        ends_with(&mut app, 1);
    }

    #[test]
    fn losing_the_realm_while_measuring_ends_the_run() {
        let (mut app, _) = measuring();
        app.world_mut().resource_mut::<CurrentRealm>().connected = false;
        ends_with(&mut app, 1);
    }

    #[test]
    fn a_csv_that_fails_while_measuring_ends_the_run() {
        let (mut app, _) = measuring();
        app.insert_resource(CsvFailed);
        ends_with(&mut app, 1);
    }

    #[test]
    fn a_csv_that_fails_in_its_last_flush_ends_the_run_with_code_one() {
        let (mut app, _) = measuring();
        for _ in 0..(MEASURE_SECONDS as u32) {
            app.update();
        }
        assert_eq!(phase(&app), AutopilotPhase::Ending { exit_code: 0 });
        // the writer flushes in the `Ending` frame, before the exit
        app.insert_resource(CsvFailed);
        app.update();
        assert_eq!(exits(&mut app), [AppExit::from_code(1)]);
    }

    #[test]
    fn the_failure_reason_is_the_rest_of_the_line_as_it_comes() {
        use clap::Parser;
        let parse = |line: &[&str]| AutopilotFailCommand::try_parse_from(line).unwrap().reason;
        assert_eq!(
            parse(&["/autopilot_fail", "sign-in", "failed:", "-1", "--x"]),
            ["sign-in", "failed:", "-1", "--x"]
        );
        assert!(parse(&["/autopilot_fail"]).is_empty());
    }

    #[test]
    fn the_exit_marker_is_rfc_3339_utc() {
        let now = utc_now();
        assert!(
            chrono::DateTime::parse_from_rfc3339(&now).is_ok() && now.ends_with('Z'),
            "{now}"
        );
    }
}
