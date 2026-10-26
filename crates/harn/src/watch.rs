use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use harn_core::Profile;
use harn_orca::{Event, WatchOptions};
use harn_update::{Agent, CheckOptions, Outcome, UpdateConfig, Updater};

use crate::cli::WatchArgs;
use crate::commands::describe;
use crate::config::{Config, Paths};

const SECS_PER_MIN: u64 = 60;
/// Floor for background update checks, so `check_interval_mins = 0` can't hammer the registry.
const MIN_BACKGROUND_INTERVAL: Duration = Duration::from_secs(5 * SECS_PER_MIN);
const WATCH_LOCK_FILE: &str = "watch.lock";

pub fn watch(args: WatchArgs, cfg: Config, paths: &Paths, profiles: &[Profile]) -> Result<i32> {
    let _instance = single_instance(&paths.cache_dir)?;
    let opts = WatchOptions {
        orca_bin: cfg.orca.bin.clone(),
        interval_ms: args.interval_ms.unwrap_or(cfg.orca.interval_ms),
        dry_run: args.dry_run,
        pinned: args.terminals,
    };
    let mut updates = (cfg.update.enabled && !args.no_update)
        .then(|| BackgroundUpdater::spawn(cfg.update.clone(), paths.cache_dir.clone()));
    // harn-orca logs attach/detach/actions itself; the CLI only drives updates from ticks.
    let mut on_event = |event: &Event| {
        if let (Some(updates), Event::Tick { active_agents }) = (updates.as_mut(), event) {
            updates.on_tick(active_agents);
        }
    };
    // Ctrl+C simply terminates the process: the watcher holds nothing that needs cleanup.
    let stop = AtomicBool::new(false);
    tracing::info!(
        dry_run = opts.dry_run,
        interval_ms = opts.interval_ms,
        "watching Orca terminals"
    );
    harn_orca::watch(&opts, profiles, &cfg.policy, &mut on_event, &stop)
        .context("cannot watch Orca terminals (is Orca running and `orca` on PATH?)")?;
    Ok(0)
}

/// Two watchers would both type `continue` into the same terminal. The OS drops the lock when the
/// process dies, so a crash never leaves a stale lock behind.
fn single_instance(cache_dir: &Path) -> Result<File> {
    fs::create_dir_all(cache_dir)
        .with_context(|| format!("cannot create {}", cache_dir.display()))?;
    let path = cache_dir.join(WATCH_LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => bail!("another `harn watch` is already running"),
        Err(TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("cannot lock {}", path.display()))
        }
    }
}

/// Updates agents between sessions on a worker thread, so a slow `npm i -g` never stalls the watcher.
/// Only agents without a live terminal are updated: on Windows a running binary cannot be replaced.
struct BackgroundUpdater {
    requests: Sender<&'static Agent>,
    finished: Receiver<&'static str>,
    in_flight: HashSet<&'static str>,
    last_request: HashMap<&'static str, Instant>,
    interval: Duration,
}

impl BackgroundUpdater {
    fn spawn(cfg: UpdateConfig, cache_dir: PathBuf) -> Self {
        let configured = Duration::from_secs(cfg.check_interval_mins.saturating_mul(SECS_PER_MIN));
        let (requests, rx) = mpsc::channel::<&'static Agent>();
        let (done, finished) = mpsc::channel::<&'static str>();
        thread::spawn(move || {
            let updater = Updater::new(cfg, cache_dir);
            for agent in rx {
                match updater.ensure_latest(agent, CheckOptions::default()) {
                    Ok(Outcome::Skipped { reason }) => {
                        tracing::debug!(agent = agent.id, %reason, "update skipped")
                    }
                    Ok(outcome) => tracing::info!("{}", describe(agent.id, &outcome)),
                    Err(e) => tracing::warn!(agent = agent.id, error = %e, "update failed"),
                }
                if done.send(agent.id).is_err() {
                    break;
                }
            }
        });
        Self {
            requests,
            finished,
            in_flight: HashSet::new(),
            last_request: HashMap::new(),
            interval: configured.max(MIN_BACKGROUND_INTERVAL),
        }
    }

    fn on_tick(&mut self, active_agents: &[String]) {
        while let Ok(id) = self.finished.try_recv() {
            self.in_flight.remove(id);
        }
        for agent in harn_update::known_agents() {
            let busy =
                self.in_flight.contains(agent.id) || active_agents.iter().any(|a| a == agent.id);
            let due = self
                .last_request
                .get(agent.id)
                .is_none_or(|at| at.elapsed() >= self.interval);
            if busy || !due {
                continue;
            }
            self.last_request.insert(agent.id, Instant::now());
            self.in_flight.insert(agent.id);
            if self.requests.send(agent).is_err() {
                tracing::warn!(
                    agent = agent.id,
                    "update worker stopped; background updates disabled"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_watcher_is_refused_until_the_first_exits() {
        let dir = std::env::temp_dir().join(format!("harn-watch-lock-{}", std::process::id()));
        let first = single_instance(&dir).unwrap();
        let err = single_instance(&dir).unwrap_err();
        assert!(err.to_string().contains("already running"), "{err:#}");
        drop(first);
        assert!(single_instance(&dir).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }
}
