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
mod tests {
    use super::*;

    fn parse(src: &str) -> Result<Policy, toml::de::Error> {
        toml::from_str(src)
    }

    #[test]
    fn defaults_match_spec() {
        let p = Policy::default();
        assert_eq!(
            (
                p.settle_ms,
                p.effect_timeout_ms,
                p.recovery_ms,
                p.stall_after_ms,
                p.user_grace_ms
            ),
            (4_000, 90_000, 120_000, 900_000, 30_000)
        );
        assert_eq!(p.max_actions_per_hour, 40);
        let r = &p.reactions;
        let shape = |r: &Reaction| (r.action, r.delay_secs, r.max_delay_secs, r.max_attempts);
        use ReactionKind::{Continue, Ignore, Notify};
        assert_eq!(shape(&r.overloaded), (Continue, 20, 600, 30));
        assert_eq!(shape(&r.server_error), (Continue, 20, 600, 20));
        assert_eq!(shape(&r.network), (Continue, 10, 300, 30));
        assert_eq!(shape(&r.rate_limited), (Continue, 60, 900, 20));
        assert_eq!(shape(&r.usage_limit), (Continue, 60, 1800, 50));
        assert_eq!(r.usage_limit.fallback_delay_secs, Some(900));
        assert_eq!(r.stalled.action, Notify);
        assert_eq!(r.stopped_early.action, Ignore);
    }

    #[test]
    fn empty_config_is_default() {
        assert_eq!(parse("").unwrap(), Policy::default());
    }

    #[test]
    fn partial_reaction_keeps_that_triggers_defaults() {
        let p = parse("settle_ms = 1000\n[reactions.overloaded]\naction = \"notify\"\n").unwrap();
        let d = Policy::default();
        assert_eq!(p.settle_ms, 1000);
        assert_eq!(p.effect_timeout_ms, d.effect_timeout_ms);
        assert_eq!(
            p.reactions.overloaded,
            Reaction {
                action: ReactionKind::Notify,
                ..d.reactions.overloaded.clone()
            }
        );
        assert_eq!(p.reactions.server_error, d.reactions.server_error);
    }

    #[test]
    fn partial_usage_limit_keeps_its_own_defaults() {
        let p = parse("[reactions.usage_limit]\ntext = \"go on\"\nmax_attempts = 5\n").unwrap();
        let u = &p.reactions.usage_limit;
        assert_eq!(u.text.as_deref(), Some("go on"));
        assert_eq!(u.max_attempts, 5);
        assert_eq!((u.delay_secs, u.max_delay_secs), (60, 1800));
        assert_eq!(u.fallback_delay_secs, Some(900));
    }

    #[test]
    fn unknown_keys_and_bad_values_are_rejected() {
        assert!(parse("[reactions.overloaded]\nactoin = \"notify\"\n").is_err());
        assert!(parse("[reactions.overlaoded]\naction = \"notify\"\n").is_err());
        assert!(parse("[reactions.overloaded]\naction = \"retry\"\n").is_err());
        assert!(parse("settle = 1\n").is_err());
    }

    fn too_small(field: &str, value: u64, min: u64) -> Result<(), PolicyError> {
        Err(PolicyError::TooSmall {
            field: field.into(),
            value,
            min,
        })
    }

    #[test]
    fn defaults_validate() {
        assert_eq!(Policy::default().validate(), Ok(()));
    }

    #[test]
    fn validate_enforces_floors() {
        let p = Policy {
            settle_ms: 499,
            ..Policy::default()
        };
        assert_eq!(p.validate(), too_small("settle_ms", 499, 500));

        let p = Policy {
            effect_timeout_ms: 3_999,
            ..Policy::default()
        };
        assert_eq!(p.validate(), too_small("effect_timeout_ms", 3_999, 4_000));

        let p = Policy {
            max_actions_per_hour: 0,
            ..Policy::default()
        };
        assert_eq!(p.validate(), too_small("max_actions_per_hour", 0, 1));

        let mut p = Policy::default();
        p.reactions.overloaded.delay_secs = 0;
        assert_eq!(
            p.validate(),
            too_small("reactions.overloaded.delay_secs", 0, 1)
        );

        let mut p = Policy::default();
        p.reactions.network.max_delay_secs = 5;
        assert_eq!(
            p.validate(),
            too_small("reactions.network.max_delay_secs", 5, 10)
        );

        let mut p = Policy::default();
        p.reactions.usage_limit.max_attempts = 0;
        assert_eq!(
            p.validate(),
            too_small("reactions.usage_limit.max_attempts", 0, 1)
        );

        let mut p = Policy::default();
        p.reactions.usage_limit.fallback_delay_secs = Some(0);
        let field = "reactions.usage_limit.fallback_delay_secs";
        assert_eq!(p.validate(), too_small(field, 0, 1));
    }

    #[test]
    fn validate_ignores_reactions_that_never_type() {
        let mut p = Policy::default();
        p.reactions.overloaded.action = ReactionKind::Notify;
        p.reactions.overloaded.delay_secs = 0;
        p.reactions.overloaded.max_attempts = 0;
        assert_eq!(p.validate(), Ok(()));
        // A typing stall reaction fires once at stall_after_ms; its zero delays are fine.
        p.reactions.stalled.action = ReactionKind::Interrupt;
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn policy_error_names_the_field() {
        let p = Policy {
            settle_ms: 100,
            ..Policy::default()
        };
        let message = p.validate().unwrap_err().to_string();
        assert_eq!(
            message,
            "policy: settle_ms = 100 is below the minimum of 500"
        );
    }

    #[test]
    fn get_maps_every_trigger() {
        let r = Reactions::default();
        assert_eq!(r.get(Trigger::Overloaded), &r.overloaded);
        assert_eq!(r.get(Trigger::ServerError), &r.server_error);
        assert_eq!(r.get(Trigger::Network), &r.network);
        assert_eq!(r.get(Trigger::RateLimited), &r.rate_limited);
        assert_eq!(r.get(Trigger::UsageLimit), &r.usage_limit);
        assert_eq!(r.get(Trigger::Stalled), &r.stalled);
        assert_eq!(r.get(Trigger::StoppedEarly), &r.stopped_early);
    }
}
