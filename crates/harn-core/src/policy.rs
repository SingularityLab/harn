//! What to do about each trigger, and the global safety limits.

use serde::Deserialize;

use crate::classify::Trigger;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReactionKind {
    /// Type the continue text and submit.
    Continue,
    /// Press the interrupt key (Esc), then continue.
    Interrupt,
    Notify,
    Ignore,
}

impl ReactionKind {
    /// True for reactions that type into the agent's terminal.
    pub fn types_input(self) -> bool {
        matches!(self, Self::Continue | Self::Interrupt)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reaction {
    pub action: ReactionKind,
    /// Overrides the profile's continue text.
    #[serde(default)]
    pub text: Option<String>,
    pub delay_secs: u64,
    pub max_delay_secs: u64,
    pub max_attempts: u32,
    /// Usage limits only: first backoff delay when the screen gives no usable reset time
    /// (doubles per attempt up to `max_delay_secs`). `None` = `delay_secs`.
    #[serde(default)]
    pub fallback_delay_secs: Option<u64>,
}

impl Reaction {
    fn new(action: ReactionKind, delay_secs: u64, max_delay_secs: u64, max_attempts: u32) -> Self {
        Self {
            action,
            text: None,
            delay_secs,
            max_delay_secs,
            max_attempts,
            fallback_delay_secs: None,
        }
    }
}

/// Deserializes from a partial table: every missing trigger or field keeps that trigger's default.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "PartialReactions")]
pub struct Reactions {
    pub overloaded: Reaction,
    pub server_error: Reaction,
    pub network: Reaction,
    pub rate_limited: Reaction,
    pub usage_limit: Reaction,
    pub stalled: Reaction,
    pub stopped_early: Reaction,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub settle_ms: u64,
    pub effect_timeout_ms: u64,
    pub recovery_ms: u64,
    pub stall_after_ms: u64,
    pub user_grace_ms: u64,
    pub max_actions_per_hour: u32,
    pub reactions: Reactions,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error("policy: {field} = {value} is below the minimum of {min}")]
    TooSmall { field: String, value: u64, min: u64 },
}

/// Floors that keep a hand-edited policy from hammering an agent or never letting it act.
const MIN_SETTLE_MS: u64 = 500;
const MIN_DELAY_SECS: u64 = 1;
const MIN_ATTEMPTS: u64 = 1;
const MIN_ACTIONS_PER_HOUR: u64 = 1;

impl Policy {
    /// Checks the floors a config file must respect; call it after loading one.
    pub fn validate(&self) -> Result<(), PolicyError> {
        at_least("settle_ms", self.settle_ms, MIN_SETTLE_MS)?;
        at_least("effect_timeout_ms", self.effect_timeout_ms, self.settle_ms)?;
        let per_hour = u64::from(self.max_actions_per_hour);
        at_least("max_actions_per_hour", per_hour, MIN_ACTIONS_PER_HOUR)?;
        let typing = self
            .reactions
            .with_backoff()
            .into_iter()
            .filter(|(_, r)| r.action.types_input());
        for (name, reaction) in typing {
            let field = |key: &str| format!("reactions.{name}.{key}");
            at_least(&field("delay_secs"), reaction.delay_secs, MIN_DELAY_SECS)?;
            let max_delay = reaction.max_delay_secs;
            at_least(&field("max_delay_secs"), max_delay, reaction.delay_secs)?;
            let attempts = u64::from(reaction.max_attempts);
            at_least(&field("max_attempts"), attempts, MIN_ATTEMPTS)?;
            if let Some(fallback) = reaction.fallback_delay_secs {
                at_least(&field("fallback_delay_secs"), fallback, MIN_DELAY_SECS)?;
            }
        }
        Ok(())
    }
}

fn at_least(field: &str, value: u64, min: u64) -> Result<(), PolicyError> {
    if value >= min {
        return Ok(());
    }
    Err(PolicyError::TooSmall {
        field: field.to_owned(),
        value,
        min,
    })
}

impl Reactions {
    pub fn get(&self, trigger: Trigger) -> &Reaction {
        match trigger {
            Trigger::Overloaded => &self.overloaded,
            Trigger::ServerError => &self.server_error,
            Trigger::Network => &self.network,
            Trigger::RateLimited => &self.rate_limited,
            Trigger::UsageLimit => &self.usage_limit,
            Trigger::Stalled => &self.stalled,
            Trigger::StoppedEarly => &self.stopped_early,
        }
    }

    /// Reactions scheduled with delays and attempts, by config key. A stall reacts once, at
    /// `stall_after_ms`, so its delays and attempts are unused.
    fn with_backoff(&self) -> [(&'static str, &Reaction); 6] {
        [
            ("overloaded", &self.overloaded),
            ("server_error", &self.server_error),
            ("network", &self.network),
            ("rate_limited", &self.rate_limited),
            ("usage_limit", &self.usage_limit),
            ("stopped_early", &self.stopped_early),
        ]
    }
}

impl Default for Reactions {
    fn default() -> Self {
        use ReactionKind::{Continue, Ignore, Notify};
        Self {
            overloaded: Reaction::new(Continue, 20, 600, 30),
            server_error: Reaction::new(Continue, 20, 600, 20),
            network: Reaction::new(Continue, 10, 300, 30),
            rate_limited: Reaction::new(Continue, 60, 900, 20),
            // delay = margin after the parsed reset time; unparsed → 900 s doubling to 1800 s.
            usage_limit: Reaction {
                fallback_delay_secs: Some(900),
                ..Reaction::new(Continue, 60, 1800, 50)
            },
            stalled: Reaction::new(Notify, 0, 0, 1),
            stopped_early: Reaction::new(Ignore, 30, 300, 10),
        }
    }
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            settle_ms: 4_000,
            effect_timeout_ms: 90_000,
            recovery_ms: 120_000,
            stall_after_ms: 900_000,
            user_grace_ms: 30_000,
            max_actions_per_hour: 40,
            reactions: Reactions::default(),
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PartialReactions {
    overloaded: PartialReaction,
    server_error: PartialReaction,
    network: PartialReaction,
    rate_limited: PartialReaction,
    usage_limit: PartialReaction,
    stalled: PartialReaction,
    stopped_early: PartialReaction,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PartialReaction {
    action: Option<ReactionKind>,
    text: Option<String>,
    delay_secs: Option<u64>,
    max_delay_secs: Option<u64>,
    max_attempts: Option<u32>,
    fallback_delay_secs: Option<u64>,
}

impl PartialReaction {
    fn over(self, base: Reaction) -> Reaction {
        Reaction {
            action: self.action.unwrap_or(base.action),
            text: self.text.or(base.text),
            delay_secs: self.delay_secs.unwrap_or(base.delay_secs),
            max_delay_secs: self.max_delay_secs.unwrap_or(base.max_delay_secs),
            max_attempts: self.max_attempts.unwrap_or(base.max_attempts),
            fallback_delay_secs: self.fallback_delay_secs.or(base.fallback_delay_secs),
        }
    }
}

impl From<PartialReactions> for Reactions {
    fn from(p: PartialReactions) -> Self {
        let d = Reactions::default();
        Self {
            overloaded: p.overloaded.over(d.overloaded),
            server_error: p.server_error.over(d.server_error),
            network: p.network.over(d.network),
            rate_limited: p.rate_limited.over(d.rate_limited),
            usage_limit: p.usage_limit.over(d.usage_limit),
            stalled: p.stalled.over(d.stalled),
            stopped_early: p.stopped_early.over(d.stopped_early),
        }
    }
}

#[cfg(test)]
mod tests;
