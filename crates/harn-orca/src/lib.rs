//! Orca ADE sidecar: watches agent terminals through the `orca` CLI and nudges stalled ones.
//!
//! Every `interval_ms` the watcher lists Orca's terminals, re-reads the rendered screen of each agent
//! terminal whose output moved (or whose engine has a deadline due), feeds it to a
//! [`harn_core::Engine`] and types the engine's input with `orca terminal send`. A terminal whose
//! screen ends in a shell prompt is treated as exited, whatever Orca calls it; Enter is pressed only
//! when the agent's input box shows exactly the text just typed.

mod cli;
mod screen;
mod watcher;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use harn_core::{Action, Policy, Profile};
use tracing::warn;

pub use cli::CliError;

use watcher::{Agents, CoreAgents, Watcher};

pub const DEFAULT_INTERVAL_MS: u64 = 2000;
/// Each `orca` call takes hundreds of ms; polling faster only queues them up.
const MIN_INTERVAL_MS: u64 = 100;
/// An hour between polls already makes the watcher useless; larger values are config mistakes.
const MAX_INTERVAL_MS: u64 = 60 * 60 * 1000;
/// How quickly `watch` notices `stop` while sleeping between cycles.
const STOP_POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct WatchOptions {
    /// Path or name of the `orca` executable.
    pub orca_bin: PathBuf,
    pub interval_ms: u64,
    /// Log actions instead of sending them.
    pub dry_run: bool,
    /// Watch only these terminals, assuming the given agent for each instead of Orca's
    /// `agentIdentity`. Empty: every terminal Orca identifies as a known agent.
    pub pinned: Vec<TerminalPin>,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            orca_bin: PathBuf::from("orca"),
            interval_ms: DEFAULT_INTERVAL_MS,
            dry_run: false,
            pinned: Vec::new(),
        }
    }
}

/// A terminal to watch regardless of what Orca says runs in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalPin {
    /// Orca terminal handle (`term_...`).
    pub handle: String,
    /// Profile id or alias of the agent in it, e.g. `claude`.
    pub agent: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// An engine started for the terminal: its screen was read and is not a shell prompt.
    Attached { handle: String, agent: String },
    /// The terminal vanished, changed agent, or its agent exited back to a shell.
    Detached { handle: String },
    /// An engine action was carried out (or only logged, with `dry_run`). Refused or failed typing
    /// is logged and not reported.
    Acted {
        handle: String,
        agent: String,
        action: Action,
        dry_run: bool,
    },
    /// Agent ids that may be running (emitted once per poll cycle): every listed terminal whose agent
    /// is known, watched or not, except those showing a shell prompt. Not emitted when the terminal
    /// list cannot be fetched.
    Tick { active_agents: Vec<String> },
}

#[derive(Debug, thiserror::Error)]
pub enum OrcaError {
    #[error("invalid watch options: {0}")]
    InvalidOptions(String),
    #[error("cannot reach Orca: {0}")]
    Unreachable(CliError),
}

/// Runs until `stop` is set. Per-terminal failures are logged and skipped; only a failure to reach
/// Orca at all on the first cycle is returned as an error.
pub fn watch(
    opts: &WatchOptions,
    profiles: &[Profile],
    policy: &Policy,
    on_event: &mut dyn FnMut(&Event),
    stop: &AtomicBool,
) -> Result<(), OrcaError> {
    let agents = CoreAgents {
        profiles: profiles.to_vec(),
        policy: policy.clone(),
    };
    validate(opts, &agents)?;
    let cli = cli::ProcessCli::new(opts.orca_bin.clone());
    let mut watcher = Watcher::new(cli, agents, opts.dry_run, opts.pinned.clone());
    let mut first_cycle = true;
    while !stop.load(Ordering::Relaxed) {
        let next_cycle = Instant::now() + Duration::from_millis(opts.interval_ms);
        match watcher.tick(epoch_ms()) {
            Ok(events) => events.iter().for_each(&mut *on_event),
            Err(error) if first_cycle => return Err(OrcaError::Unreachable(error)),
            Err(error) => warn!(%error, "cannot list Orca terminals; retrying next cycle"),
        }
        first_cycle = false;
        while !stop.load(Ordering::Relaxed) && Instant::now() < next_cycle {
            thread::sleep(
                next_cycle
                    .saturating_duration_since(Instant::now())
                    .min(STOP_POLL),
            );
        }
    }
    Ok(())
}

fn validate(opts: &WatchOptions, agents: &impl Agents) -> Result<(), OrcaError> {
    let invalid = |why: String| Err(OrcaError::InvalidOptions(why));
    if !(MIN_INTERVAL_MS..=MAX_INTERVAL_MS).contains(&opts.interval_ms) {
        let range = format!("{MIN_INTERVAL_MS}..={MAX_INTERVAL_MS}");
        return invalid(format!(
            "interval_ms {} is outside {range}",
            opts.interval_ms
        ));
    }
    for pin in &opts.pinned {
        if cli::terminal_arg(&pin.handle).is_err() {
            return invalid(format!("{:?} is not an Orca terminal handle", pin.handle));
        }
        if agents.profile_id(&pin.agent).is_none() {
            return invalid(format!("no profile for agent {:?}", pin.agent));
        }
    }
    Ok(())
}

fn epoch_ms() -> u64 {
    // Only a host clock set before 1970 fails here; 0 then just keeps every screen unsettled.
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since_epoch.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(opts: WatchOptions) -> Result<(), OrcaError> {
        let opts = WatchOptions {
            orca_bin: PathBuf::from("orca-binary-that-does-not-exist"),
            ..opts
        };
        let stop = AtomicBool::new(false);
        let mut on_event = |_: &Event| stop.store(true, Ordering::Relaxed);
        watch(
            &opts,
            &Profile::builtins(),
            &Policy::default(),
            &mut on_event,
            &stop,
        )
    }

    #[test]
    fn rejects_invalid_options_and_unreachable_orca() {
        let pin = |handle: &str, agent: &str| TerminalPin {
            handle: handle.to_owned(),
            agent: agent.to_owned(),
        };
        for opts in [
            WatchOptions {
                interval_ms: 0,
                ..WatchOptions::default()
            },
            WatchOptions {
                interval_ms: u64::MAX,
                ..WatchOptions::default()
            },
            WatchOptions {
                pinned: vec![pin("", "claude")],
                ..WatchOptions::default()
            },
            WatchOptions {
                pinned: vec![pin("term_a", "nope")],
                ..WatchOptions::default()
            },
        ] {
            let err = run(opts.clone()).unwrap_err();
            assert!(
                matches!(err, OrcaError::InvalidOptions(_)),
                "{opts:?}: {err:?}"
            );
        }
        let err = run(WatchOptions::default()).unwrap_err();
        assert!(
            matches!(err, OrcaError::Unreachable(CliError::Spawn { .. })),
            "{err:?}"
        );
    }
}
