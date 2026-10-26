//! What the tray shows, derived from watcher reports. No IO: time comes in as arguments.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use harn_core::Action;
use harn_orca::Event;
use harn_update::Outcome;

/// When Orca goes away mid-session harn-orca keeps retrying quietly; the only sign is that ticks
/// stop arriving.
const MIN_STALE_AFTER: Duration = Duration::from_secs(30);
const STALE_AFTER_CYCLES: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Colour {
    Green,
    Grey,
    Amber,
    Red,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Paused,
    Connecting,
    OrcaDown,
    Watching { sessions: usize },
    NeedsYou { agent: String },
    Stopped { reason: String },
}

impl Status {
    pub fn colour(&self) -> Colour {
        match self {
            Self::Paused => Colour::Grey,
            Self::Connecting | Self::OrcaDown => Colour::Amber,
            Self::Watching { .. } => Colour::Green,
            Self::NeedsYou { .. } | Self::Stopped { .. } => Colour::Red,
        }
    }

    pub fn text(&self) -> String {
        match self {
            Self::Paused => "Paused".to_owned(),
            Self::Connecting => "Connecting to Orca…".to_owned(),
            Self::OrcaDown => "Orca not running — retrying".to_owned(),
            Self::Watching { sessions: 0 } => "Watching — no agent sessions".to_owned(),
            Self::Watching { sessions: 1 } => "Watching 1 agent session".to_owned(),
            Self::Watching { sessions } => format!("Watching {sessions} agent sessions"),
            Self::NeedsYou { agent } => format!("{agent} needs you"),
            Self::Stopped { reason } => format!("Stopped: {reason}"),
        }
    }
}

/// What the watcher thread reports to the tray.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    Event(Event),
    /// Orca could not be reached; the watcher retries by itself.
    Unreachable,
    /// The watcher gave up for good (invalid options or profiles).
    Stopped(String),
}

#[derive(Debug)]
enum Phase {
    Paused,
    Connecting,
    OrcaDown,
    Running { last_tick: Instant },
    Stopped(String),
}

#[derive(Debug)]
pub struct TrayState {
    phase: Phase,
    stale_after: Duration,
    /// handle -> agent of every attached session.
    sessions: BTreeMap<String, String>,
    /// handle -> agent of sessions harn handed over to the user, until the user opens the menu.
    attention: BTreeMap<String, String>,
    active_agents: Vec<String>,
    last: Option<String>,
}

impl TrayState {
    pub fn new(poll_interval: Duration) -> Self {
        let mut state = Self {
            phase: Phase::Connecting,
            stale_after: MIN_STALE_AFTER,
            sessions: BTreeMap::new(),
            attention: BTreeMap::new(),
            active_agents: Vec::new(),
            last: None,
        };
        state.started(poll_interval);
        state
    }

    /// A new watcher knows nothing of the previous one's sessions.
    pub fn started(&mut self, poll_interval: Duration) {
        self.phase = Phase::Connecting;
        self.stale_after = poll_interval
            .saturating_mul(STALE_AFTER_CYCLES)
            .max(MIN_STALE_AFTER);
        self.sessions.clear();
        self.attention.clear();
    }

    pub fn paused(&mut self) {
        self.phase = Phase::Paused;
        self.sessions.clear();
        self.attention.clear();
    }

    /// The user opened the menu, so they have seen what needed them.
    pub fn acknowledge(&mut self) {
        self.attention.clear();
    }

    pub fn apply(&mut self, report: Report, now: Instant, clock: &str) {
        match report {
            Report::Event(event) => self.on_event(event, now, clock),
            Report::Unreachable => {
                self.phase = Phase::OrcaDown;
                self.sessions.clear();
            }
            Report::Stopped(reason) => {
                self.phase = Phase::Stopped(reason);
                self.sessions.clear();
            }
        }
    }

    fn on_event(&mut self, event: Event, now: Instant, clock: &str) {
        match event {
            Event::Attached { handle, agent } => {
                self.sessions.insert(handle, agent);
            }
            Event::Detached { handle } => {
                self.sessions.remove(&handle);
                self.attention.remove(&handle);
            }
            Event::Tick { active_agents } => {
                self.phase = Phase::Running { last_tick: now };
                self.active_agents = active_agents;
            }
            Event::Acted {
                handle,
                agent,
                action,
                dry_run: false,
            } => self.on_action(handle, agent, &action, clock),
            Event::Acted { .. } => {}
        }
    }

    fn on_action(&mut self, handle: String, agent: String, action: &Action, clock: &str) {
        let what = match action {
            Action::Input { .. } => {
                // harn is handling this terminal again, so it no longer waits for the user.
                self.attention.remove(&handle);
                format!("continued {agent}")
            }
            Action::Notify { trigger, .. } => {
                let what = format!("{agent} needs you ({trigger})");
                self.attention.insert(handle, agent);
                what
            }
        };
        self.last = Some(format!("{what} at {clock}"));
    }

    pub fn record_update(&mut self, agent: &str, outcome: &Outcome, clock: &str) {
        if let Outcome::Updated { to, .. } = outcome {
            self.last = Some(format!("updated {agent} to {to} at {clock}"));
        }
    }

    /// Agents with a live session as of the last poll; background updates leave them alone.
    pub fn active_agents(&self) -> &[String] {
        &self.active_agents
    }

    pub fn status(&self, now: Instant) -> Status {
        let watching = match &self.phase {
            Phase::Paused => return Status::Paused,
            Phase::Stopped(reason) => {
                return Status::Stopped {
                    reason: reason.clone(),
                };
            }
            Phase::Connecting => Status::Connecting,
            Phase::OrcaDown => Status::OrcaDown,
            Phase::Running { last_tick }
                if now.saturating_duration_since(*last_tick) > self.stale_after =>
            {
                Status::OrcaDown
            }
            Phase::Running { .. } => Status::Watching {
                sessions: self.sessions.len(),
            },
        };
        match self.attention.values().next() {
            Some(agent) => Status::NeedsYou {
                agent: agent.clone(),
            },
            None => watching,
        }
    }

    pub fn last_line(&self) -> String {
        match &self.last {
            Some(last) => format!("Last: {last}"),
            None => "Last: nothing yet".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests;
