//! Per-session state machine: snapshots in, actions out.

mod stall;
mod typing;

use std::collections::VecDeque;

use jiff::tz::TimeZone;

use crate::Snapshot;
use crate::classify::{Classification, InputBox, Problem, Trigger, scan};
use crate::policy::{Policy, Reaction, ReactionKind};
use crate::profile::Profile;
use stall::StallWatch;
use typing::Acted;

const SECOND_MS: u64 = 1_000;
const HOUR_MS: u64 = 3_600_000;
/// After `continue`, the echoed prompt plus at least one reply row push the handled line this far up.
const SCROLLED_LINES: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Write `chunks` to the agent's terminal in order, sleeping `gap_ms` between them.
    Input {
        chunks: Vec<String>,
        gap_ms: u64,
        trigger: Trigger,
        attempt: u32,
    },
    Notify {
        trigger: Trigger,
        message: String,
    },
}

#[derive(Debug)]
enum Phase {
    Clear,
    /// Seen on a settled screen; act at `due_ms` if it is still shown.
    Scheduled {
        problem: Problem,
        due_ms: u64,
    },
    /// Dealt with: typed into (`acted`), notified, ignored, given up on, or the agent moved on by
    /// itself. `rearmed` = the agent worked since, so the same line in the same place is a new
    /// occurrence rather than the old one still sitting there.
    Handled {
        problem: Problem,
        acted: Option<Acted>,
        rearmed: bool,
    },
}

impl Phase {
    fn handled(problem: Problem) -> Self {
        Self::Handled {
            problem,
            acted: None,
            rearmed: false,
        }
    }
}

#[derive(Debug)]
pub struct Engine {
    profile: Profile,
    policy: Policy,
    tz: TimeZone,
    phase: Phase,
    /// Actions taken in the current run of failures.
    attempts: u32,
    working_since_ms: Option<u64>,
    stall: StallWatch,
    /// When recent `Input` actions were emitted, for `max_actions_per_hour`.
    inputs: VecDeque<u64>,
    user_input_ms: Option<u64>,
    /// The agent's input box on the last screen; typing needs it seen empty.
    input: InputBox,
    /// When the last screen will count as settled, if it did not yet.
    settle_end_ms: Option<u64>,
}

impl Engine {
    /// Uses the system time zone for parsing usage-limit reset times.
    pub fn new(profile: Profile, policy: Policy) -> Self {
        Self::with_time_zone(profile, policy, TimeZone::system())
    }

    pub fn with_time_zone(profile: Profile, policy: Policy, tz: TimeZone) -> Self {
        Self {
            profile,
            policy,
            tz,
            phase: Phase::Clear,
            attempts: 0,
            working_since_ms: None,
            stall: StallWatch::default(),
            inputs: VecDeque::new(),
            user_input_ms: None,
            input: InputBox::Missing,
            settle_end_ms: None,
        }
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    pub fn step(&mut self, snap: &Snapshot, now_ms: u64) -> Vec<Action> {
        let scan = scan(&self.profile, &snap.text, now_ms, &self.tz);
        self.input = scan.input;
        let settle_end = snap.last_output_ms.saturating_add(self.policy.settle_ms);
        let settled = now_ms >= settle_end;
        self.settle_end_ms = (!settled).then_some(settle_end);
        self.inputs
            .retain(|&at| now_ms.saturating_sub(at) < HOUR_MS);

        if scan.class == Classification::Working {
            self.on_working(now_ms);
            return self.check_stall(&snap.text, now_ms).into_iter().collect();
        }
        self.working_since_ms = None;
        self.stall.reset();
        if !settled {
            return Vec::new();
        }
        if let Some(action) = self.handle_unsent(now_ms) {
            return vec![action];
        }
        match scan.class {
            Classification::Problem(problem) => self.on_problem(problem, now_ms),
            Classification::Idle | Classification::Working => {
                self.phase = Phase::Clear;
                self.attempts = 0;
                Vec::new()
            }
        }
    }

    /// The user typed into the agent; automation pauses for `policy.user_grace_ms`.
    pub fn note_user_input(&mut self, now_ms: u64) {
        self.user_input_ms = Some(now_ms);
    }

    /// Earliest epoch ms at which `step` may act even if the screen does not change.
    /// Adapters that only re-read on output changes use this to wake up.
    pub fn next_deadline_ms(&self) -> Option<u64> {
        let settle = self.settle_end_ms.unwrap_or(0);
        let phase = match &self.phase {
            Phase::Scheduled { problem, due_ms } => {
                Some((*due_ms).max(settle).max(self.gate(problem.trigger)))
            }
            Phase::Handled {
                acted: Some(acted),
                rearmed: false,
                ..
            } => {
                let timeout = acted.at_ms.saturating_add(self.policy.effect_timeout_ms);
                let draft = self.input.draft();
                let unsent = acted.unsent(draft, self.policy.settle_ms, self.keys_gate());
                Some(
                    unsent
                        .map_or(timeout, |(_, at)| at.min(timeout))
                        .max(settle),
                )
            }
            Phase::Clear | Phase::Handled { .. } => None,
        };
        let stall = self
            .stall
            .stalled_at_ms(self.policy.stall_after_ms)
            .filter(|_| self.reaction(Trigger::Stalled).action != ReactionKind::Ignore)
            .map(|at| at.max(self.gate(Trigger::Stalled)));
        [phase, stall, self.settle_end_ms]
            .into_iter()
            .flatten()
            .filter(|&at| at != u64::MAX)
            .min()
    }

    fn on_working(&mut self, now_ms: u64) {
        let since = *self.working_since_ms.get_or_insert(now_ms);
        if now_ms.saturating_sub(since) >= self.policy.recovery_ms {
            self.attempts = 0;
        }
        self.phase = match std::mem::replace(&mut self.phase, Phase::Clear) {
            Phase::Clear => Phase::Clear,
            Phase::Scheduled { problem, .. } | Phase::Handled { problem, .. } => Phase::Handled {
                problem,
                acted: None,
                rearmed: true,
            },
        };
    }

    fn on_problem(&mut self, problem: Problem, now_ms: u64) -> Vec<Action> {
        let (mark, acted_at_ms, rearmed) = match &self.phase {
            Phase::Clear => return self.new_occurrence(problem, now_ms),
            Phase::Scheduled { problem: mark, .. } => (mark.clone(), None, false),
            Phase::Handled {
                problem: mark,
                acted,
                rearmed,
            } => (mark.clone(), acted.as_ref().map(|a| a.at_ms), *rearmed),
        };
        let same_line = mark.trigger == problem.trigger && mark.line == problem.line;
        if same_line && problem.lines_below >= mark.lines_below + SCROLLED_LINES {
            // Output appeared below a line we already know: it is history, and proof the agent moved.
            self.phase = Phase::Handled {
                problem: mark,
                acted: None,
                rearmed: true,
            };
            return Vec::new();
        }
        if let Phase::Scheduled {
            problem: pending, ..
        } = &mut self.phase
            && pending.trigger == problem.trigger
        {
            // Still the scheduled problem; track where it is now for the scroll check.
            *pending = problem;
            return self.fire_if_due(now_ms);
        }
        let waiting_for_effect =
            matches!(self.phase, Phase::Handled { .. }) && same_line && !rearmed;
        if !waiting_for_effect {
            return self.new_occurrence(problem, now_ms);
        }
        let timed_out = acted_at_ms
            .is_some_and(|at| now_ms >= at.saturating_add(self.policy.effect_timeout_ms));
        if timed_out {
            self.new_occurrence(problem, now_ms)
        } else {
            Vec::new()
        }
    }

    fn new_occurrence(&mut self, problem: Problem, now_ms: u64) -> Vec<Action> {
        let reaction = self.reaction(problem.trigger);
        let due_ms = match reaction.action {
            ReactionKind::Ignore => {
                self.phase = Phase::handled(problem);
                return Vec::new();
            }
            ReactionKind::Notify => now_ms.saturating_add(secs(reaction.delay_secs)),
            ReactionKind::Continue | ReactionKind::Interrupt => {
                let attempt = self.attempts.saturating_add(1);
                if attempt > reaction.max_attempts {
                    let detail = format!(
                        "still failing after {} attempts, leaving it to you: {}",
                        reaction.max_attempts, problem.line
                    );
                    let action = self.notify(problem.trigger, &detail);
                    self.phase = Phase::handled(problem);
                    return vec![action];
                }
                due_ms(&reaction, &problem, attempt, now_ms)
            }
        };
        self.phase = Phase::Scheduled { problem, due_ms };
        self.fire_if_due(now_ms)
    }

    fn fire_if_due(&mut self, now_ms: u64) -> Vec<Action> {
        let Phase::Scheduled { problem, due_ms } = &self.phase else {
            return Vec::new();
        };
        if now_ms < *due_ms || now_ms < self.gate(problem.trigger) {
            return Vec::new();
        }
        let problem = problem.clone();
        let reaction = self.reaction(problem.trigger);
        if reaction.action.types_input() {
            self.attempts = self.attempts.saturating_add(1);
            self.inputs.push_back(now_ms);
            let text = self.continue_text(&reaction);
            let acted = Acted::new(now_ms, self.attempts, &text);
            let action = self.input(&reaction, text, problem.trigger, self.attempts);
            self.phase = Phase::Handled {
                problem,
                acted: Some(acted),
                rearmed: false,
            };
            return vec![action];
        }
        let action = (reaction.action == ReactionKind::Notify).then(|| {
            let typing_wanted = self
                .policy
                .reactions
                .get(problem.trigger)
                .action
                .types_input();
            let detail = if typing_wanted {
                format!(
                    "{} (notify only: harn cannot see this agent's input box)",
                    problem.line
                )
            } else {
                problem.line.clone()
            };
            self.notify(problem.trigger, &detail)
        });
        self.phase = Phase::handled(problem);
        action.into_iter().collect()
    }

    fn check_stall(&mut self, text: &str, now_ms: u64) -> Option<Action> {
        self.stall.observe(text, now_ms);
        let due = self.stall.stalled_at_ms(self.policy.stall_after_ms)?;
        if now_ms < due || now_ms < self.gate(Trigger::Stalled) {
            return None;
        }
        self.stall.mark_reported();
        let reaction = self.reaction(Trigger::Stalled);
        match reaction.action {
            ReactionKind::Ignore => None,
            ReactionKind::Notify => {
                let secs = self.policy.stall_after_ms / SECOND_MS;
                let detail = format!("screen unchanged for {secs} s while working");
                Some(self.notify(Trigger::Stalled, &detail))
            }
            ReactionKind::Continue | ReactionKind::Interrupt => {
                self.inputs.push_back(now_ms);
                let text = self.continue_text(&reaction);
                Some(self.input(&reaction, text, Trigger::Stalled, 1))
            }
        }
    }

    fn notify(&self, trigger: Trigger, detail: &str) -> Action {
        Action::Notify {
            trigger,
            message: format!("{}: {trigger}: {detail}", self.profile.name),
        }
    }
}

/// When to act on 1-based `attempt`: after the parsed reset time plus `delay_secs` while that is
/// still ahead, otherwise `min(base * 2^(attempt-1), max_delay_secs)` from now.
fn due_ms(reaction: &Reaction, problem: &Problem, attempt: u32, now_ms: u64) -> u64 {
    if let Some(reset) = problem.resets_at_ms {
        let after_reset = reset.saturating_add(secs(reaction.delay_secs));
        if after_reset > now_ms {
            return after_reset;
        }
    }
    let base = match (problem.trigger, reaction.fallback_delay_secs) {
        (Trigger::UsageLimit, Some(fallback)) => fallback,
        _ => reaction.delay_secs,
    };
    let factor = 2u64.saturating_pow(attempt.saturating_sub(1));
    let delay = base.saturating_mul(factor).min(reaction.max_delay_secs);
    now_ms.saturating_add(secs(delay))
}

fn secs(secs: u64) -> u64 {
    secs.saturating_mul(SECOND_MS)
}

#[cfg(test)]
mod tests;
