//! One SIGINT / SIGTERM policy for every command, so Ctrl-C always ends the process.
//!
//! The moment any code polls `tokio::signal::ctrl_c()`, tokio owns SIGINT for the whole
//! process: a signal nobody happens to be awaiting is swallowed, and a second Ctrl-C while
//! a teardown stalls does nothing. `start` listened cooperatively and then killed two
//! process groups, each of which may block for a second; `host` and the signing flows never
//! listened at all. So there is exactly one owner, installed first thing by `main`:
//!
//! - a signal with a cooperative listener armed (`shutdown_signal` is being awaited) is
//!   theirs: they tear down and return, and `main` exits. A grace period starts; if the
//!   teardown is still running when it ends, the process exits anyway. That is for a
//!   sender that signals once and waits -- a script, `timeout`, a test harness, systemd --
//!   since a person just presses Ctrl-C again;
//! - a second signal exits at once;
//! - a signal with no listener armed exits at once, which is what the default action did
//!   before tokio took the signal over, now with the child process groups killed too.
//!
//! Every exit here runs the child-group kills, because `kill_on_drop` fires on a clean drop
//! only and `process::exit` drops nothing.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

/// The teardown after a signal is bounded by construction: two process-group kills that
/// each wait `KILL_GRACE` for SIGTERM before SIGKILL, and drops that kill without waiting.
/// Anything past that is a hang, not slowness, so this is those two waits plus slack.
pub const GRACE: Duration = Duration::from_secs(2 * crate::asset_bundles::KILL_GRACE.as_secs() + 1);
pub const EXIT_CODE: i32 = 130;

static ARMED: AtomicUsize = AtomicUsize::new(0);
/// A listener consumed the latest signal. Set before its arm count drops, because tokio
/// wakes every listener on the same signal and the cooperative one can finish, and disarm,
/// before the watchdog gets to look: seen as a hard 130 exit on a plain Ctrl-C.
static CLAIMED: AtomicBool = AtomicBool::new(false);
static SIGNALS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// A cooperative listener will tear down and return; exit if it takes longer than `GRACE`.
    Grace,
    ExitNow,
}

/// The decision for the `nth` signal (1-based) given whether a cooperative listener is
/// armed for it or has already consumed it.
pub fn decide(nth: usize, listening: bool) -> Action {
    if nth >= 2 || !listening {
        Action::ExitNow
    } else {
        Action::Grace
    }
}

/// Resolves on SIGINT (Ctrl-C) or, on unix, SIGTERM. Counts as an armed listener for as
/// long as it is being awaited.
pub async fn shutdown_signal() {
    listen(&ARMED, wait()).await
}

/// Awaits `signal` counted in `armed`, and marks the signal claimed once it resolves.
async fn listen(armed: &AtomicUsize, signal: impl Future<Output = ()>) {
    struct Armed<'a>(&'a AtomicUsize);
    impl Drop for Armed<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    armed.fetch_add(1, Ordering::SeqCst);
    let guard = Armed(armed);
    signal.await;
    CLAIMED.store(true, Ordering::SeqCst);
    drop(guard);
}

async fn wait() {
    #[cfg(unix)]
    let term = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()).ok() {
            Some(mut t) => {
                t.recv().await;
            }
            None => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term => {}
    }
}

/// A signal arrived in this process. True from the moment a cooperative
/// listener resolves: the listener claims the signal before it returns, and
/// the watchdog counts the signal before it takes the claim.
pub fn signalled() -> bool {
    CLAIMED.load(Ordering::SeqCst) || SIGNALS.load(Ordering::SeqCst) > 0
}

/// Kill what would otherwise outlive us, then exit.
pub fn exit_now(reason: &str) -> ! {
    crate::ux::note_stderr(format!("stopping now: {reason}"));
    crate::asset_bundles::kill_sidecar_group();
    crate::livekit_server::kill_group();
    std::process::exit(EXIT_CODE)
}

/// Install the watchdog. Call once, from `main`, before any command runs.
pub fn install() {
    tokio::spawn(async {
        loop {
            wait().await;
            let nth = SIGNALS.fetch_add(1, Ordering::SeqCst) + 1;
            let listening =
                ARMED.load(Ordering::SeqCst) > 0 || CLAIMED.swap(false, Ordering::SeqCst);
            match decide(nth, listening) {
                Action::ExitNow if nth >= 2 => exit_now("second interrupt"),
                Action::ExitNow => exit_now("interrupted"),
                Action::Grace => {
                    tokio::spawn(async {
                        tokio::time::sleep(GRACE).await;
                        exit_now(&format!("shutdown did not finish within {GRACE:?}"));
                    });
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_first_signal_is_the_listeners_to_handle_and_a_second_one_exits() {
        assert_eq!(decide(1, true), Action::Grace);
        assert_eq!(decide(2, true), Action::ExitNow);
        assert_eq!(decide(3, true), Action::ExitNow);
    }

    #[test]
    fn a_signal_nobody_is_listening_for_exits_at_once() {
        assert_eq!(decide(1, false), Action::ExitNow);
    }

    // Its own counter: the start tests await shutdown_signal on ARMED concurrently.
    #[tokio::test]
    async fn awaiting_the_signal_arms_the_watchdog_and_dropping_it_disarms() {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let mut listener = Box::pin(listen(&COUNT, std::future::pending()));
        assert!(
            futures::poll!(&mut listener).is_pending(),
            "no signal was sent, so the listener must still be waiting"
        );
        assert_eq!(COUNT.load(Ordering::SeqCst), 1);
        drop(listener);
        assert_eq!(COUNT.load(Ordering::SeqCst), 0);
    }
}
