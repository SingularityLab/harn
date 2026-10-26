//! Agent profiles: how to recognise each agent's states on screen and how to talk to it.

use regex::Regex;
use serde::Deserialize;

use crate::classify::Trigger;

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("invalid profile TOML: {0}")]
    Toml(#[from] toml::de::Error),
}

/// A regex compiled while deserializing, so a bad user profile fails at load time.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "String")]
pub struct Pattern(pub Regex);

impl TryFrom<String> for Pattern {
    type Error = regex::Error;

    fn try_from(src: String) -> Result<Self, Self::Error> {
        Regex::new(&src).map(Pattern)
    }
}

impl Pattern {
    pub fn is_match(&self, haystack: &str) -> bool {
        self.0.is_match(haystack)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub id: String,
    pub name: String,
    /// Command basenames and Orca `agentIdentity` values that select this profile.
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default = "default_continue_text")]
    pub continue_text: String,
    #[serde(default = "default_submit")]
    pub submit: String,
    #[serde(default = "default_interrupt")]
    pub interrupt: String,
    /// Pause between input chunks; Ink-based TUIs treat text+Enter in one write as a paste.
    #[serde(default = "default_gap_ms")]
    pub gap_ms: u64,
    /// Non-blank lines at the bottom of the screen that count as "current".
    #[serde(default = "default_tail_lines")]
    pub tail_lines: usize,
    pub signatures: Signatures,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Signatures {
    pub working: Vec<Pattern>,
    pub overloaded: Vec<Pattern>,
    pub server_error: Vec<Pattern>,
    pub network: Vec<Pattern>,
    pub rate_limited: Vec<Pattern>,
    pub usage_limit: Vec<Pattern>,
    pub stopped_early: Vec<Pattern>,
    /// Regexes with named groups describing when a usage limit resets: `days`/`hours`/`minutes`/
    /// `seconds` (relative), `hour`/`minute`/`ampm` with optional `month`/`day`/`year`/`tz`
    /// (wall clock), or `epoch` (unix seconds).
    pub reset_hints: Vec<Pattern>,
    /// Rows that echo a user message (`❯ say hi`). The last one on screen is the live input row;
    /// problems are only looked for between the one before it and the input row, so errors that
    /// were already answered (e.g. by an echoed `continue`) never count again.
    pub user_echo: Vec<Pattern>,
    /// The live input row when it holds no draft (bare prompt or placeholder). Empty = no draft
    /// detection.
    pub empty_input: Vec<Pattern>,
    /// Rows that are never agent output (rules, turn-finished lines, tips). Skipped when checking
    /// whether the agent's output ends with a `stopped_early` question.
    pub chrome: Vec<Pattern>,
}

impl Signatures {
    /// True if the profile can find the live input row and tell an empty one from a draft:
    /// without that, harn never types into this agent.
    pub(crate) fn locates_input(&self) -> bool {
        !self.user_echo.is_empty() && !self.empty_input.is_empty()
    }

    /// Patterns for a trigger the classifier can see on a single screen (`Stalled` has none).
    pub(crate) fn for_trigger(&self, trigger: Trigger) -> &[Pattern] {
        match trigger {
            Trigger::Overloaded => &self.overloaded,
            Trigger::ServerError => &self.server_error,
            Trigger::Network => &self.network,
            Trigger::RateLimited => &self.rate_limited,
            Trigger::UsageLimit => &self.usage_limit,
            Trigger::StoppedEarly => &self.stopped_early,
            Trigger::Stalled => &[],
        }
    }
}

pub(crate) fn any_match(patterns: &[Pattern], haystack: &str) -> bool {
    patterns.iter().any(|p| p.is_match(haystack))
}

fn default_continue_text() -> String {
    "continue".into()
}
fn default_submit() -> String {
    "\r".into()
}
fn default_interrupt() -> String {
    "\u{1b}".into()
}
fn default_gap_ms() -> u64 {
    150
}
fn default_tail_lines() -> usize {
    15
}

const BUILTIN_SOURCES: [&str; 4] = [
    include_str!("../profiles/claude.toml"),
    include_str!("../profiles/codex.toml"),
    include_str!("../profiles/gemini.toml"),
    include_str!("../profiles/opencode.toml"),
];

/// Launcher extensions stripped from command names before matching (npm shims on Windows).
const COMMAND_EXTENSIONS: [&str; 4] = ["exe", "cmd", "bat", "ps1"];

impl Profile {
    pub fn from_toml(src: &str) -> Result<Self, ProfileError> {
        Ok(toml::from_str(src)?)
    }

    /// Built-in profiles (claude, codex, gemini, opencode), embedded at compile time.
    #[allow(
        clippy::expect_used,
        reason = "embedded at compile time; the `builtins_parse` test proves every source parses"
    )]
    pub fn builtins() -> Vec<Profile> {
        BUILTIN_SOURCES
            .iter()
            .map(|src| Profile::from_toml(src).expect("built-in profile must parse"))
            .collect()
    }

    /// True if `name` (a command basename like `claude.exe` or an Orca agent identity) selects this profile.
    pub fn matches(&self, name: &str) -> bool {
        let key = command_key(name);
        !key.is_empty()
            && std::iter::once(&self.id)
                .chain(&self.aliases)
                .any(|candidate| candidate.eq_ignore_ascii_case(key))
    }
}

/// `C:\npm\claude.CMD` → `claude`, `/usr/bin/codex` → `codex`, `Claude Code` → `Claude Code`.
fn command_key(name: &str) -> &str {
    let base = name.trim().rsplit(['/', '\\']).next().unwrap_or_default();
    match base.rsplit_once('.') {
        Some((stem, ext))
            if COMMAND_EXTENSIONS
                .iter()
                .any(|e| e.eq_ignore_ascii_case(ext)) =>
        {
            stem
        }
        _ => base,
    }
}

#[cfg(test)]
mod tests;
