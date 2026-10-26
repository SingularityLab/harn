//! Desktop notifications (Windows toasts, macOS Notification Center, Linux D-Bus).

use harn_core::Action;
use harn_update::Outcome;
use serde::Deserialize;

const APP_NAME: &str = "harn";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotifyConfig {
    pub enabled: bool,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone)]
pub struct Notifier {
    enabled: bool,
}

impl Notifier {
    pub fn new(cfg: &NotifyConfig) -> Self {
        Self {
            enabled: cfg.enabled,
        }
    }

    /// A failing notification is logged and otherwise ignored: it must never stop harn's real work.
    pub fn send(&self, (title, body): (String, String)) {
        if !self.enabled {
            return;
        }
        let shown = notify_rust::Notification::new()
            .appname(APP_NAME)
            .summary(&title)
            .body(&body)
            .show();
        if let Err(error) = shown {
            tracing::warn!(%error, %title, "cannot show a desktop notification");
        }
    }
}

/// What to tell the user about an engine action. Only the first nudge of a run of failures and
/// anything that needs the user are worth a popup; later attempts would just be noise.
pub fn for_action(agent: &str, action: &Action) -> Option<(String, String)> {
    match action {
        Action::Input {
            trigger,
            attempt: 1,
            ..
        } => Some((
            format!("{agent}: {trigger}"),
            "harn typed `continue` for you.".to_owned(),
        )),
        Action::Input { .. } => None,
        Action::Notify { trigger, message } => {
            Some((format!("{agent} needs you ({trigger})"), message.clone()))
        }
    }
}

pub fn for_update(agent: &str, outcome: &Outcome) -> Option<(String, String)> {
    match outcome {
        Outcome::Updated { from, to } => {
            Some((format!("{agent} updated"), format!("{from} -> {to}")))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use harn_core::Trigger;

    use super::*;

    fn input(attempt: u32) -> Action {
        Action::Input {
            chunks: vec!["continue".into(), "\r".into()],
            gap_ms: 0,
            trigger: Trigger::Overloaded,
            attempt,
        }
    }

    #[test]
    fn only_first_nudges_and_calls_for_help_pop_up() {
        let (title, _) = for_action("claude", &input(1)).unwrap();
        assert_eq!(title, "claude: overloaded");
        assert_eq!(for_action("claude", &input(2)), None);
        let help = Action::Notify {
            trigger: Trigger::UsageLimit,
            message: "gave up".into(),
        };
        assert_eq!(
            for_action("codex", &help),
            Some(("codex needs you (usage limit)".into(), "gave up".into()))
        );
    }

    #[test]
    fn only_real_updates_pop_up() {
        let updated = Outcome::Updated {
            from: "1.0.0".into(),
            to: "1.1.0".into(),
        };
        assert!(for_update("codex", &updated).is_some());
        let same = Outcome::UpToDate {
            version: "1.1.0".into(),
        };
        assert_eq!(for_update("codex", &same), None);
    }
}
