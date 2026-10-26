//! Runs `harn_orca::watch` on its own thread and reports what happens to the tray.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use harn_core::{Policy, Profile};
use harn_orca::{Event, OrcaError, WatchOptions};

use crate::state::Report;

/// Orca not running is normal (it may simply not be open yet), so retries are slow and quiet.
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// How quickly a retry wait notices that the watcher should stop.
const STOP_POLL: Duration = Duration::from_millis(100);

pub struct Watcher {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl Watcher {
    pub fn spawn(
        opts: WatchOptions,
        profiles: Vec<Profile>,
        policy: Policy,
        report: impl Fn(Report) + Send + 'static,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("harn-watch".to_owned())
            .spawn(move || watch_until_stopped(&opts, &profiles, &policy, &report, &flag))
            .context("cannot start the watcher thread")?;
        Ok(Self { stop, thread })
    }

    /// Blocks until the thread has finished, so a new watcher never types into a terminal while
    /// this one still might.
    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        if self.thread.join().is_err() {
            tracing::error!("the watcher thread panicked");
        }
    }
}

fn watch_until_stopped(
    opts: &WatchOptions,
    profiles: &[Profile],
    policy: &Policy,
    report: &dyn Fn(Report),
    stop: &AtomicBool,
) {
    let mut on_event = |event: &Event| report(Report::Event(event.clone()));
    let mut logged_unreachable = false;
    while !stop.load(Ordering::Relaxed) {
        match harn_orca::watch(opts, profiles, policy, &mut on_event, stop) {
            Ok(()) => return,
            Err(OrcaError::Unreachable(error)) => {
                if logged_unreachable {
                    tracing::debug!(%error, "Orca is still not reachable");
                } else {
                    tracing::info!(%error, retry_secs = RETRY_AFTER.as_secs(), "Orca is not reachable; retrying");
                    logged_unreachable = true;
                }
                report(Report::Unreachable);
                sleep_unless_stopped(RETRY_AFTER, stop);
            }
            Err(error) => {
                tracing::error!(%error, "stopped watching Orca terminals");
                report(Report::Stopped(error.to_string()));
                return;
            }
        }
    }
}

fn sleep_unless_stopped(duration: Duration, stop: &AtomicBool) {
    let until = Instant::now() + duration;
    while !stop.load(Ordering::Relaxed) {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        thread::sleep(left.min(STOP_POLL));
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::mpsc;

    use super::*;

    fn opts(orca_bin: &str, interval_ms: u64) -> WatchOptions {
        WatchOptions {
            orca_bin: PathBuf::from(orca_bin),
            interval_ms,
            ..WatchOptions::default()
        }
    }

    fn spawn(opts: WatchOptions) -> (Watcher, mpsc::Receiver<Report>) {
        let (tx, rx) = mpsc::channel();
        let report = move |r| tx.send(r).unwrap();
        let watcher = Watcher::spawn(opts, Profile::builtins(), Policy::default(), report).unwrap();
        (watcher, rx)
    }

    #[test]
    fn missing_orca_is_reported_and_stop_interrupts_the_retry_wait() {
        let (watcher, rx) = spawn(opts("orca-binary-that-does-not-exist", 2000));
        let first = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(first, Report::Unreachable);
        let started = Instant::now();
        watcher.stop();
        assert!(started.elapsed() < RETRY_AFTER / 2);
    }

    #[test]
    fn invalid_options_stop_the_watcher_for_good() {
        let (watcher, rx) = spawn(opts("orca", 0));
        let report = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(matches!(report, Report::Stopped(reason) if reason.contains("interval_ms")));
        watcher.stop();
    }
}
