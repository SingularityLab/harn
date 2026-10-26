use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use harn_update::{Agent, CheckOptions, Outcome, UpdateConfig, Updater};

const SECS_PER_MIN: u64 = 60;
/// Floor for background update checks, so `check_interval_mins = 0` can't hammer the registry.
const MIN_BACKGROUND_INTERVAL: Duration = Duration::from_secs(5 * SECS_PER_MIN);

pub fn describe(agent: &str, outcome: &Outcome) -> String {
    match outcome {
        Outcome::UpToDate { version } => format!("{agent}: up to date ({version})"),
        Outcome::Updated { from, to } => format!("{agent}: updated {from} -> {to}"),
        Outcome::Available { from, to } => format!("{agent}: update available {from} -> {to}"),
        Outcome::Skipped { reason } => format!("{agent}: skipped ({reason})"),
    }
}

/// Updates agents between sessions on a worker thread, so a slow `npm i -g` never stalls the caller.
/// Only agents without a live session are updated: on Windows a running binary cannot be replaced.
pub struct BackgroundUpdater {
    requests: Sender<&'static Agent>,
    finished: Receiver<&'static str>,
    in_flight: HashSet<&'static str>,
    last_request: HashMap<&'static str, Instant>,
    interval: Duration,
}

impl BackgroundUpdater {
    /// `on_outcome` runs on the worker thread after every finished check or install.
    pub fn spawn(
        cfg: UpdateConfig,
        cache_dir: PathBuf,
        on_outcome: impl Fn(&'static Agent, &Outcome) + Send + 'static,
    ) -> Self {
        let configured = Duration::from_secs(cfg.check_interval_mins.saturating_mul(SECS_PER_MIN));
        let (requests, rx) = mpsc::channel::<&'static Agent>();
        let (done, finished) = mpsc::channel::<&'static str>();
        thread::spawn(move || {
            let updater = Updater::new(cfg, cache_dir);
            for agent in rx {
                match updater.ensure_latest(agent, CheckOptions::default()) {
                    Ok(outcome) => {
                        if let Outcome::Skipped { reason } = &outcome {
                            tracing::debug!(agent = agent.id, %reason, "update skipped");
                        } else {
                            tracing::info!("{}", describe(agent.id, &outcome));
                        }
                        on_outcome(agent, &outcome);
                    }
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

    /// Requests a check for every idle agent whose interval has passed.
    pub fn on_tick(&mut self, active_agents: &[String]) {
        self.request(active_agents, false);
    }

    /// Requests a check for every idle agent right away.
    pub fn update_now(&mut self, active_agents: &[String]) {
        self.request(active_agents, true);
    }

    fn request(&mut self, active_agents: &[String], ignore_interval: bool) {
        while let Ok(id) = self.finished.try_recv() {
            self.in_flight.remove(id);
        }
        for agent in harn_update::known_agents() {
            let busy =
                self.in_flight.contains(agent.id) || active_agents.iter().any(|a| a == agent.id);
            let due = ignore_interval
                || self
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
