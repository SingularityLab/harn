//! Single-snapshot classification of an agent's screen.

mod reset;

use std::fmt;
use std::ops::Range;

use jiff::tz::TimeZone;
use serde::Deserialize;

use crate::profile::{Profile, Signatures, any_match};

/// Why the agent needs a nudge. Also the key of a reaction in the policy config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    Overloaded,
    ServerError,
    Network,
    RateLimited,
    UsageLimit,
    Stalled,
    StoppedEarly,
}

impl fmt::Display for Trigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Overloaded => "overloaded",
            Self::ServerError => "server error",
            Self::Network => "network error",
            Self::RateLimited => "rate limited",
            Self::UsageLimit => "usage limit",
            Self::Stalled => "stalled",
            Self::StoppedEarly => "stopped early",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub trigger: Trigger,
    /// The screen line that matched, trimmed.
    pub line: String,
    /// Non-blank lines between the matched line and the input row (the bottom of the screen when
    /// the profile cannot locate one), so footer and pane-size changes do not move it.
    pub lines_below: usize,
    /// Epoch ms when a usage limit resets, if the screen says so.
    pub resets_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    Working,
    Idle,
    Problem(Problem),
}

/// Error triggers, highest precedence first (when one line matches several).
const PRECEDENCE: [Trigger; 5] = [
    Trigger::UsageLimit,
    Trigger::RateLimited,
    Trigger::Overloaded,
    Trigger::ServerError,
    Trigger::Network,
];

/// Rows after a usage-limit row that may still hold its reset time (the TUI wraps long messages).
const RESET_CONTEXT_LINES: usize = 2;

/// Trailing output rows joined before matching `stopped_early`, so a wrapped question still matches.
const QUESTION_CONTEXT_LINES: usize = 2;

pub fn classify(profile: &Profile, text: &str, now_ms: u64, tz: &TimeZone) -> Classification {
    scan(profile, text, now_ms, tz).class
}

/// A classification plus what the agent's input box holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Scan {
    pub(crate) class: Classification,
    pub(crate) input: InputBox,
}

/// The agent's input box as far as the screen tells. Only `Empty` is safe to type into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputBox {
    /// The profile has no input-row signatures.
    Unknown,
    /// The profile knows the input row but it is not on screen: the agent exited to a shell, or a
    /// dialog or menu replaced the prompt.
    Missing,
    Empty,
    /// Unsent text, without the prompt glyph (`❯ fix the tests` → `fix the tests`).
    Draft(String),
}

impl InputBox {
    pub(crate) fn draft(&self) -> Option<&str> {
        match self {
            Self::Draft(text) => Some(text),
            _ => None,
        }
    }
}

pub(crate) fn scan(profile: &Profile, text: &str, now_ms: u64, tz: &TimeZone) -> Scan {
    let tail = tail(text, profile.tail_lines);
    let sigs = &profile.signatures;
    let row = tail.iter().rposition(|row| any_match(&sigs.user_echo, row));
    let input = match row {
        _ if !sigs.locates_input() => InputBox::Unknown,
        None => InputBox::Missing,
        Some(i) if any_match(&sigs.empty_input, tail[i]) => InputBox::Empty,
        Some(i) => InputBox::Draft(draft_text(sigs, tail[i])),
    };
    // Anywhere on screen: the agent's own retry banners carry error text but mean "busy".
    if tail.iter().any(|row| any_match(&sigs.working, row)) {
        return Scan {
            class: Classification::Working,
            input,
        };
    }
    let turn = current_turn(sigs, &tail, row);
    let class = last_error(sigs, &tail, turn.clone(), now_ms, tz)
        .or_else(|| stopped_early(sigs, &tail, turn))
        .map_or(Classification::Idle, Classification::Problem);
    Scan { class, input }
}

fn draft_text(sigs: &Signatures, row: &str) -> String {
    let start = sigs
        .user_echo
        .iter()
        .find_map(|p| p.0.find(row))
        .map_or(0, |m| m.end());
    row.get(start..).unwrap_or_default().trim().to_owned()
}

/// The last `n` non-blank lines.
fn tail(text: &str, n: usize) -> Vec<&str> {
    let mut lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines.drain(..lines.len().saturating_sub(n));
    lines
}

/// Rows after the last echoed user message and above the live input row: the agent's output
/// since the user (or harn) last spoke. Without echo signatures, the whole tail.
fn current_turn(sigs: &Signatures, tail: &[&str], input: Option<usize>) -> Range<usize> {
    let Some(input) = input else {
        return 0..tail.len();
    };
    let start = tail[..input]
        .iter()
        .rposition(|row| any_match(&sigs.user_echo, row))
        .map_or(0, |i| i + 1);
    start..input
}

fn last_error(
    sigs: &Signatures,
    tail: &[&str],
    turn: Range<usize>,
    now_ms: u64,
    tz: &TimeZone,
) -> Option<Problem> {
    let end = turn.end;
    turn.rev().find_map(|i| {
        let trigger = PRECEDENCE
            .into_iter()
            .find(|t| any_match(sigs.for_trigger(*t), tail[i]))?;
        let resets_at_ms = if trigger == Trigger::UsageLimit {
            let context = &tail[i..(i + 1 + RESET_CONTEXT_LINES).min(end)];
            reset::parse(&sigs.reset_hints, &join(context), now_ms, tz)
        } else {
            None
        };
        Some(Problem {
            trigger,
            line: tail[i].trim().to_owned(),
            lines_below: end - i - 1,
            resets_at_ms,
        })
    })
}

/// A question at the very end of the agent's output (chrome rows below it are skipped).
fn stopped_early(sigs: &Signatures, tail: &[&str], turn: Range<usize>) -> Option<Problem> {
    let end = turn.end;
    let output: Vec<usize> = turn
        .filter(|&i| !any_match(&sigs.chrome, tail[i]))
        .collect();
    let &last = output.last()?;
    let question: Vec<&str> = output[output.len().saturating_sub(QUESTION_CONTEXT_LINES)..]
        .iter()
        .map(|&i| tail[i])
        .collect();
    any_match(&sigs.stopped_early, &join(&question)).then(|| Problem {
        trigger: Trigger::StoppedEarly,
        line: tail[last].trim().to_owned(),
        lines_below: end - last - 1,
        resets_at_ms: None,
    })
}

fn join(rows: &[&str]) -> String {
    rows.iter()
        .map(|row| row.trim())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests;
