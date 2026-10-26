//! Keystrokes: what we type into the agent, when typing is allowed (fail closed: only into an
//! input box seen empty), and recovering from an Enter the agent swallowed.

use super::{Action, Engine, HOUR_MS, Phase};
use crate::classify::{InputBox, Trigger};
use crate::policy::{Reaction, ReactionKind};

/// Enter re-sends per action when our own text is left unsent in the input box.
const MAX_RESUBMITS: u32 = 2;

/// What we typed for an occurrence: times its effect and spots a swallowed Enter.
#[derive(Debug)]
pub(super) struct Acted {
    pub(super) at_ms: u64,
    attempt: u32,
    /// The continue text, trimmed like a scanned draft.
    text: String,
    last_sent_ms: u64,
    resubmits: u32,
    gave_up: bool,
}

/// The next move while our own text sits unsent in the input box.
pub(super) enum Unsent {
    Resubmit,
    GiveUp,
}

impl Acted {
    pub(super) fn new(at_ms: u64, attempt: u32, text: &str) -> Self {
        Self {
            at_ms,
            attempt,
            text: text.trim().to_owned(),
            last_sent_ms: at_ms,
            resubmits: 0,
            gave_up: false,
        }
    }

    /// What to do about our text still unsent in the input box, and from when: press Enter
    /// again (`MAX_RESUBMITS` times), then tell the user once. Each step waits `settle_ms` after
    /// the previous keystrokes so they can show an effect.
    pub(super) fn unsent(
        &self,
        draft: Option<&str>,
        settle_ms: u64,
        keys_open: u64,
    ) -> Option<(Unsent, u64)> {
        if self.gave_up || draft != Some(self.text.as_str()) {
            return None;
        }
        let spaced = self.last_sent_ms.saturating_add(settle_ms);
        Some(if self.resubmits < MAX_RESUBMITS {
            (Unsent::Resubmit, spaced.max(keys_open))
        } else {
            (Unsent::GiveUp, spaced)
        })
    }
}

impl Engine {
    /// Our text is still unsent in the input box and the agent never started: the Enter was
    /// swallowed (Orca's send warns it can be). Presses Enter alone again a bounded number of
    /// times, then notifies once and leaves the box alone.
    pub(super) fn handle_unsent(&mut self, now_ms: u64) -> Option<Action> {
        let keys_open = self.keys_gate();
        let Phase::Handled {
            problem,
            acted: Some(acted),
            rearmed: false,
        } = &mut self.phase
        else {
            return None;
        };
        let (step, at) = acted.unsent(self.input.draft(), self.policy.settle_ms, keys_open)?;
        if now_ms < at {
            return None;
        }
        acted.last_sent_ms = now_ms;
        let trigger = problem.trigger;
        match step {
            Unsent::Resubmit => {
                acted.resubmits = acted.resubmits.saturating_add(1);
                self.inputs.push_back(now_ms);
                Some(Action::Input {
                    chunks: vec![self.profile.submit.clone()],
                    gap_ms: self.profile.gap_ms,
                    trigger,
                    attempt: acted.attempt,
                })
            }
            Unsent::GiveUp => {
                acted.gave_up = true;
                let detail = format!(
                    "harn typed `{}` but the agent did not submit it",
                    acted.text
                );
                Some(self.notify(trigger, &detail))
            }
        }
    }

    /// The configured reaction, except that typing degrades to a notification when the profile
    /// cannot locate the agent's input box: harn could never tell an empty prompt from a draft,
    /// a dialog or a shell.
    pub(super) fn reaction(&self, trigger: Trigger) -> Reaction {
        let mut reaction = self.policy.reactions.get(trigger).clone();
        if reaction.action.types_input() && !self.profile.signatures.locates_input() {
            reaction.action = ReactionKind::Notify;
        }
        reaction
    }

    /// Earliest time this trigger's reaction may run (possibly past); `u64::MAX` = not until the
    /// screen changes. Only typing is gated: by an input box not seen empty, then `keys_gate`.
    pub(super) fn gate(&self, trigger: Trigger) -> u64 {
        if !self.reaction(trigger).action.types_input() {
            return 0;
        }
        if self.input != InputBox::Empty {
            return u64::MAX;
        }
        self.keys_gate()
    }

    /// Earliest time any keystroke may be sent: after the user-input grace, within the hourly cap.
    pub(super) fn keys_gate(&self) -> u64 {
        let cap = usize::try_from(self.policy.max_actions_per_hour).unwrap_or(usize::MAX);
        if cap == 0 {
            return u64::MAX;
        }
        let grace_end = self
            .user_input_ms
            .map_or(0, |at| at.saturating_add(self.policy.user_grace_ms));
        let cap_free = self
            .inputs
            .len()
            .checked_sub(cap)
            .and_then(|oldest| self.inputs.get(oldest))
            .map_or(0, |at| at.saturating_add(HOUR_MS));
        grace_end.max(cap_free)
    }

    pub(super) fn continue_text(&self, reaction: &Reaction) -> String {
        reaction
            .text
            .clone()
            .unwrap_or_else(|| self.profile.continue_text.clone())
    }

    /// Continue = `[text, submit]`, interrupt = `[interrupt, text, submit]`.
    pub(super) fn input(
        &self,
        reaction: &Reaction,
        text: String,
        trigger: Trigger,
        attempt: u32,
    ) -> Action {
        let mut chunks = Vec::with_capacity(3);
        if reaction.action == ReactionKind::Interrupt {
            chunks.push(self.profile.interrupt.clone());
        }
        chunks.push(text);
        chunks.push(self.profile.submit.clone());
        Action::Input {
            chunks,
            gap_ms: self.profile.gap_ms,
            trigger,
            attempt,
        }
    }
}
