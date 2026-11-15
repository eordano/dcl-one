#![cfg_attr(not(feature = "console"), windows_subsystem = "windows")]

use std::{
    error::Error,
    fmt::Display,
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use bevy::{
    app::AppExit,
    log::{tracing_subscriber::Layer, LogPlugin},
    prelude::*,
};
use clap::Parser;
use dcl_deno_ipc::init_runtime;
use mimalloc::MiMalloc;
use perf_output::{
    layer::PerfLogLayer,
    sink::{DeferredSink, FileSink},
};
use system_ui::crash_report;
use webgpu_build::{
    serialized_app_config, DecentralandApp, DecentralandAppConfig, DecentralandArguments,
};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;
static SESSION_LOG: OnceLock<String> = OnceLock::new();
/// The benchmark log (`--log-file`). The command line is parsed after the logger starts, so the
/// layer is installed with it and holds what is logged until [`open_benchmark_log`] knows
/// where it goes.
static BENCHMARK_LOG: DeferredSink = DeferredSink::new();

fn main() {
    // As the registered `decentraland://` handler (windows/linux) the OS spawns a fresh process
    // per link: a sign-in link belongs to the running explorer, hand it over through the launcher
    // bridge file and stop here. Any other link is a place to boot into (`deep_link` below).
    if let Some(url) = platform::deeplink::signin_link_arg() {
        if let Err(e) = platform::deeplink::write_launcher_bridge(&url) {
            eprintln!("failed to relay deep link: {e}");
            std::process::exit(1);
        }
        return;
    }

    decentraland_log_file();
    create_logs_folder();
    create_log_files();

    let decentraland_app = DecentralandApp::new(LogPlugin {
        filter: "wgpu=error,naga=error,bevy_animation=error,matrix=error,symphonia=warn"
            .to_string(),
        custom_layer: move |_| {
            // appending, as `startup_error` does, so neither writes over the other's lines
            let (non_blocking, guard) = tracing_appender::non_blocking(
                File::options()
                    .append(true)
                    .open(SESSION_LOG.get().unwrap())
                    .unwrap(),
            );
            Box::leak(guard.into());
            Some(Box::new(
                bevy::log::tracing_subscriber::fmt::layer()
                    .with_writer(non_blocking)
                    .with_ansi(false)
                    .and_then(PerfLogLayer::new(&BENCHMARK_LOG)),
            ))
        },
        ..default()
    });

    // requires local version of `bevy_mod_debugdump` due to once_cell version conflict.
    // probably resolved by updating deno. TODO: add feature flag for this after bumping deno
    // bevy_mod_debugdump::print_main_schedule(&mut app);
    // a `console` build keeps the default panic hook, which prints to stderr: a panic there makes
    // no exception block in the benchmark log
    #[cfg(not(feature = "console"))]
    log_panics::init();

    // args first, so --help and a bad flag answer without spawning the scene runtime
    let exit_code = match decentraland_app_config() {
        Ok(decentraland_app_config) => {
            // initialize v8 runtime from main thread
            init_runtime().unwrap();
            // the autopilot ends its run with a code, success or failure
            let exit = decentraland_app.build(decentraland_app_config).run();
            // the app is gone, so its av threads are leaving: let them, rather than have
            // process teardown kill one inside ffmpeg
            #[cfg(feature = "ffmpeg")]
            av::video_stream::join_av_threads(std::time::Duration::from_secs(2));
            match exit {
                AppExit::Success => 0,
                AppExit::Error(code) => code.get().into(),
            }
        }
        // No need to generate a crash report if the failure is due to an user error
        Err(UserError(code)) => code,
    };

    // Graceful exits don't need to have the log sent to the analytics server
    // so we remove the touch file
    let _ = std::fs::remove_file(crash_marker());

    // after the touch file is gone: an early exit would leave it behind and the next run would
    // report the usage error as a crash
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

fn decentraland_app_config() -> Result<DecentralandAppConfig, UserError> {
    let mut arguments = decentraland_app_arguments()?;
    open_benchmark_log(
        arguments.log_file.as_deref(),
        &BENCHMARK_LOG,
        arguments.client.autopilot,
    )?;
    check_csv_folder(arguments.csv.as_deref(), arguments.client.autopilot)?;
    let app_config = serialized_app_config();
    let crash_file = decentraland_crash_file();

    // the analytics session, in the crash marker so the next run reports a crash under it
    let session_id = arguments
        .client
        .session_id
        .get_or_insert_with(|| uuid::Uuid::new_v4().to_string());
    if let Err(e) = std::fs::write(crash_marker(), session_id) {
        warn!("failed to write the session id to the crash marker: {e}");
    }

    Ok(DecentralandAppConfig::new(
        app_config, arguments, crash_file,
    ))
}

/// `--log-file`: the whole session in the benchmark harness's line format.
/// It is written beside the session log, which stays as it is: the crash report sends that
/// file's tail, and the touch file next to it marks a run that did not end gracefully; neither
/// belongs to where a benchmark's log goes. Without the flag nothing is written.
///
/// A benchmark run that cannot create its log does not start: the harness reads no log as no
/// exceptions, so the run would be scored clean. Any other launch logs the failure and goes on.
fn open_benchmark_log(
    path: Option<&std::path::Path>,
    log: &DeferredSink,
    autopilot: bool,
) -> Result<(), UserError> {
    let Some(path) = path else {
        log.close();
        return Ok(());
    };
    match FileSink::create(path) {
        Ok(sink) => {
            log.open(sink);
            Ok(())
        }
        Err(e) => {
            log.close();
            let message = format!("can't create the log file {}: {e}", path.display());
            if autopilot {
                startup_error(&message);
                Err(UserError(BENCHMARK_OUTPUT_ERROR))
            } else {
                error!("{message}");
                Ok(())
            }
        }
    }
}

/// `--csv` under the autopilot: a run whose CSV has no folder to go in does not start, as one
/// whose log cannot be created does, instead of arriving and measuring for nothing. The file
/// itself is created only when measuring begins, so a run that never arrives leaves none.
/// Without the autopilot the flag writes nothing, and nothing is checked.
fn check_csv_folder(path: Option<&std::path::Path>, autopilot: bool) -> Result<(), UserError> {
    let Some(path) = path.filter(|_| autopilot) else {
        return Ok(());
    };
    let folder = match path.parent() {
        Some(folder) if !folder.as_os_str().is_empty() => folder,
        // a bare file name goes in the working folder
        _ => std::path::Path::new("."),
    };
    if folder.is_dir() {
        return Ok(());
    }
    startup_error(&format!(
        "can't create the benchmark CSV {}: the folder {} does not exist",
        path.display(),
        folder.display()
    ));
    Err(UserError(BENCHMARK_OUTPUT_ERROR))
}

/// The reason a launch stops before the app runs. It is logged, and also appended to the session
/// log at once: the logger's writer to that file is non-blocking and the process exits right
/// after, before the writer would get to it, and a Windows build has no console to show it.
fn startup_error(message: &str) {
    error!("{message}");
    let Some(session_log) = SESSION_LOG.get() else {
        return;
    };
    if let Ok(mut file) = File::options().append(true).open(session_log) {
        let _ = writeln!(file, "ERROR {message}");
    }
}

/// The benchmark harness passes `-logFile <path>`; it is `--log-file` here: clap has no
/// multi-letter flags behind a single dash.
fn harness_spellings(
    args: impl Iterator<Item = std::ffi::OsString>,
) -> impl Iterator<Item = std::ffi::OsString> {
    args.map(|arg| {
        if arg == "-logFile" {
            "--log-file".into()
        } else {
            arg
        }
    })
}

fn decentraland_app_arguments() -> Result<DecentralandArguments, UserError> {
    let parsed = DecentralandArguments::try_parse_from(harness_spellings(std::env::args_os()));
    let mut args = match parsed {
        Ok(args) => args,
        // --help, or a bad flag: clap's own message (an error also lands in the session log via
        // tracing), and no crash report
        Err(e) => {
            if e.use_stderr() {
                error!("{e}");
            } else {
                let _ = e.print();
            }
            return Err(UserError(e.exit_code()));
        }
    };

    if let Some(link) = &args.client.deep_link {
        let place = platform::deeplink::parse_place_link(link).map_err(|e| {
            error!("{link}: {e}");
            UserError(USAGE_ERROR)
        })?;
        // explicit flags win over the link
        args.launch.realm = args.launch.realm.take().or(place.realm);
        args.launch.position = args.launch.position.take().or(place.position);
        args.launch.base_domain = args.launch.base_domain.take().or(place.base_domain);
    }

    if let Some(gateway) = &args.gateway {
        common::base_domain::set_gateway(gateway).map_err(|e| {
            error!("{e}");
            UserError(USAGE_ERROR)
        })?;
    }

    webgpu_build::launch::latch(&args.launch).map_err(|e| {
        error!("{e}");
        UserError(USAGE_ERROR)
    })?;

    if let Some(port) = args.network_devtools {
        let address = platform::devtools::start(port).map_err(|e| {
            error!("Could not start Network inspector: {e}");
            UserError(USAGE_ERROR)
        })?;
        info!("Explorer Network inspector http://{address}/json/list");
    }

    // An explicit --system-scene (a scene source, or "none" for the engine's builtin ui) opts out of the
    // react HUD entirely — the given ui scene drives instead (see lib.rs).
    args.hud = args.client.system_scene.is_none();
    if args.client.system_scene.is_none() {
        args.client.system_scene = default_ui_scene();
    }
    Ok(args)
}

/// react HUD builds default to the bundled bridge-scene static export (a file realm, loaded with
/// no server; `npm run bundle:native` in react-web generates it). Checked against the cwd
/// (packaged runs) and the compile-time checkout dir (dev `cargo run` from any directory). If
/// absent there is NO ui scene — the react HUD's built-in fallback relay covers login/chat/loading.
fn default_ui_scene() -> Option<String> {
    #[cfg(feature = "react-hud-cef")]
    {
        for root in native_hud_roots() {
            let local = root.join("assets/bridge-scene/BevyExplorerUI");
            if local.join("about").is_file() {
                return Some(local.to_string_lossy().into_owned());
            }
        }
        None
    }
    #[cfg(not(feature = "react-hud-cef"))]
    Some(String::from(
        "https://dcl-regenesislabs.github.io/bevy-ui-scene/BevyUiScene",
    ))
}

#[inline(always)]
fn data_local_dir() -> PathBuf {
    let dirs = platform::project_directories().unwrap();
    dirs.data_local_dir().to_owned()
}

fn create_logs_folder() {
    let log_dir = data_local_dir();
    std::fs::create_dir_all(log_dir).unwrap();
}

fn create_log_files() {
    File::create(SESSION_LOG.get().unwrap())
        .expect("failed to create log file")
        .write_all(format!("{}\n\n", SESSION_LOG.get().unwrap()).as_bytes())
        .expect("failed to create log file");

    File::create(crash_marker()).unwrap();
    println!("log file: {}", SESSION_LOG.get().unwrap());
}

/// Generate the file name for the log files of current instance
/// and saves it to [`SESSION_LOG`]
fn decentraland_log_file() {
    let log_dir = data_local_dir();
    let session_time: chrono::DateTime<chrono::Utc> = chrono::DateTime::from_timestamp_millis(
        web_time::SystemTime::now()
            .duration_since(web_time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64,
    )
    .unwrap();
    let session_log = log_dir.join(format!("{}.log", session_time.format("%Y%m%d-%H%M%S")));
    SESSION_LOG
        .set(session_log.to_string_lossy().into_owned())
        .unwrap();
}

/// This run's crash marker: created with the log, removed on a graceful exit.
fn crash_marker() -> PathBuf {
    crash_report::marker_of(Path::new(SESSION_LOG.get().unwrap()))
}

/// The log of an earlier run that left its crash marker behind.
fn decentraland_crash_file() -> Option<PathBuf> {
    crash_report::crashed_log(&data_local_dir(), Path::new(SESSION_LOG.get().unwrap()))
}

/// A launch that stopped before the app ran, carrying the process exit status: 0 when the user
/// asked for something and got it (`--help`), [`USAGE_ERROR`] when a flag or a value was refused.
#[derive(Debug)]
struct UserError(i32);

/// clap's exit status for a usage error, so a value this binary refuses reads to a script exactly
/// like a flag clap refuses.
const USAGE_ERROR: i32 = 2;

/// The exit status of an autopilot run stopped before the app ran because its output has nowhere
/// to go: the `--log-file` could not be created, or the `--csv` folder does not exist. It leaves
/// no CSV.
const BENCHMARK_OUTPUT_ERROR: i32 = 1;

impl Display for UserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            0 => write!(f, "Nothing to run."),
            code => write!(f, "Method failed due to user error (exit code {code})."),
        }
    }
}

impl Error for UserError {}

// Roots to search for the bundled native HUD files: cwd, the executable's directory (packaged
// layouts), and the dev checkout (`cargo run` from any directory).
#[cfg(feature = "react-hud-cef")]
fn native_hud_roots() -> Vec<std::path::PathBuf> {
    let mut roots = vec![
        std::path::PathBuf::from("."),
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")),
    ];
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
    {
        roots.push(dir);
    }
    roots
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::Path};

    use super::*;

    fn spelled(args: &[&str]) -> Vec<OsString> {
        harness_spellings(args.iter().map(OsString::from)).collect()
    }

    #[test]
    fn the_harnesss_log_file_flag_is_spelled_the_way_clap_reads_it() {
        assert_eq!(
            spelled(&["x", "-logFile", "run.log", "--autopilot"]),
            ["x", "--log-file", "run.log", "--autopilot"]
        );
    }

    #[test]
    fn only_the_exact_token_is_rewritten() {
        let untouched = [
            "x",
            "-logfile",
            "-logFile=run.log",
            "--logFile",
            "-logFiles",
            "--log-file",
            "--csv",
            "-logFile.log",
            "",
        ];
        assert_eq!(spelled(&untouched), untouched);
    }

    #[test]
    fn the_harnesss_command_line_parses() {
        let measured = spelled(&[
            "decentra-bevy",
            "-logFile",
            "out/build_1.log",
            "--autopilot",
            "--csv",
            "out/build_1.csv",
            "--realm",
            "supportteam.dcl.eth",
        ]);
        let args = DecentralandArguments::try_parse_from(measured).unwrap();
        assert!(args.client.autopilot);
        assert_eq!(args.csv.as_deref(), Some(Path::new("out/build_1.csv")));
        assert_eq!(args.log_file.as_deref(), Some(Path::new("out/build_1.log")));
        assert_eq!(args.launch.realm.as_deref(), Some("supportteam.dcl.eth"));

        // the warm-up launch names no CSV
        let warm_up = spelled(&[
            "decentra-bevy",
            "-logFile",
            "out/build_0.log",
            "--autopilot",
        ]);
        let args = DecentralandArguments::try_parse_from(warm_up).unwrap();
        assert!(args.client.autopilot && args.csv.is_none());
        assert_eq!(args.log_file.as_deref(), Some(Path::new("out/build_0.log")));
    }

    #[test]
    fn gateway_requires_a_native_flag() {
        let link = DecentralandArguments::try_parse_from([
            "explorer",
            "decentraland://?position=1,2&gateway=https://other.test",
        ])
        .unwrap();
        assert!(link.gateway.is_none());
        let args = DecentralandArguments::try_parse_from([
            "explorer",
            "--gateway",
            "https://gateway.example.test",
            "--places",
            "https://custom.test/base",
        ])
        .unwrap();
        assert_eq!(
            args.gateway.as_deref(),
            Some("https://gateway.example.test")
        );
        assert_eq!(
            args.launch.services.places.as_deref(),
            Some("https://custom.test/base")
        );
    }

    #[test]
    fn signing_follows_the_resolved_gateway_route_including_explicit_overrides() {
        use common::base_domain::{self, Service};
        let gateway = "https://gateway.example.test";
        base_domain::set_gateway(gateway).unwrap();
        base_domain::set_services([
            (Service::Places, gateway),
            (
                Service::SocialApi,
                "https://private:secret@custom.example/override",
            ),
            (
                Service::PreviewGatekeeper,
                "https://gateway.example.test/comms-gatekeeper-local/custom",
            ),
        ])
        .unwrap();
        let bases: std::collections::HashMap<_, _> =
            base_domain::hud_services().into_iter().collect();
        assert_eq!(bases["socialApi"], "https://custom.example/override");
        assert_eq!(bases["events"], format!("{gateway}/events"));
        assert_eq!(
            bases["cameraReel"],
            format!("{gateway}/camera-reel-service")
        );
        assert_eq!(bases["reels"], "https://decentraland.org/reels");
        let notices = format!(
            "{}/notifications/read?private=value",
            bases["notifications"]
        );
        assert_eq!(
            base_domain::gateway_signed_path(&notices)
                .unwrap()
                .as_deref(),
            Some("/notifications/read")
        );
        assert!(!serde_json::to_string(&bases).unwrap().contains("secret"));
        let adapter = base_domain::url(Service::CommsGatekeeper, "/get-scene-adapter");
        assert_eq!(
            base_domain::gateway_signed_path(&adapter)
                .unwrap()
                .as_deref(),
            Some("/get-scene-adapter")
        );
        let preview = base_domain::url(Service::PreviewGatekeeper, "/adapter");
        assert_eq!(
            base_domain::gateway_signed_path(&preview)
                .unwrap()
                .as_deref(),
            Some("/custom/adapter")
        );
        assert_eq!(
            base_domain::gateway_signed_path(&bases["socialApi"]).unwrap(),
            None
        );
        assert_eq!(
            base_domain::url(Service::Places, "/tiles"),
            format!("{gateway}/tiles")
        );
    }

    fn log_file_in_a_missing_folder() -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!("no_such_folder_{}", std::process::id()))
            .join("run.log")
    }

    #[test]
    fn a_benchmark_run_that_cannot_create_its_log_does_not_start() {
        let path = log_file_in_a_missing_folder();
        let log = DeferredSink::new();
        let refused = open_benchmark_log(Some(&path), &log, true).unwrap_err();
        assert_ne!(refused.0, 0);
        assert!(!path.exists(), "nothing was created");
    }

    #[test]
    fn any_other_launch_goes_on_without_the_log_it_could_not_create() {
        let path = log_file_in_a_missing_folder();
        let log = DeferredSink::new();
        assert!(open_benchmark_log(Some(&path), &log, false).is_ok());
        assert!(open_benchmark_log(None, &log, true).is_ok());
    }

    #[test]
    fn a_log_that_can_be_created_opens_for_a_benchmark_run() {
        let path = std::env::temp_dir().join(format!("benchmark_log_{}.log", std::process::id()));
        let log = DeferredSink::new();
        assert!(open_benchmark_log(Some(&path), &log, true).is_ok());
        assert!(path.exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_benchmark_run_whose_csv_has_no_folder_does_not_start() {
        let path = log_file_in_a_missing_folder().with_file_name("run.csv");
        let refused = check_csv_folder(Some(&path), true).unwrap_err();
        assert_ne!(refused.0, 0);
        assert!(!path.exists(), "nothing was created");
    }

    #[test]
    fn a_csv_folder_that_exists_starts_the_run_and_creates_nothing_yet() {
        let path = std::env::temp_dir().join(format!("benchmark_csv_{}.csv", std::process::id()));
        assert!(check_csv_folder(Some(&path), true).is_ok());
        assert!(!path.exists(), "the CSV is created when measuring begins");
        // a bare file name goes in the working folder
        assert!(check_csv_folder(Some(Path::new("run.csv")), true).is_ok());
    }

    #[test]
    fn the_csv_folder_is_not_checked_without_the_autopilot() {
        let path = log_file_in_a_missing_folder().with_file_name("run.csv");
        assert!(check_csv_folder(Some(&path), false).is_ok());
        assert!(check_csv_folder(None, true).is_ok());
    }

    #[test]
    fn network_inspection_requires_an_explicit_native_flag() {
        let link = DecentralandArguments::try_parse_from([
            "explorer",
            "decentraland://?position=1,2&networkDevtools=9223&network-devtools=9223",
        ])
        .unwrap();
        assert!(link.network_devtools.is_none());
        let args =
            DecentralandArguments::try_parse_from(["explorer", "--network-devtools", "9223"])
                .unwrap();
        assert_eq!(args.network_devtools, Some(9223));
        assert!(DecentralandArguments::try_parse_from(
            ["explorer", "--network-devtools", "65536",]
        )
        .is_err());
    }

    #[test]
    fn the_flag_as_the_harness_spells_it_is_not_clap_s_own() {
        // why `harness_spellings` exists
        assert!(DecentralandArguments::try_parse_from(["x", "-logFile", "run.log"]).is_err());
    }
}
