//! "Settings…" and "Open log": the config file and the log, opened in the user's editor.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

/// Written only when there is no config yet. The two toggles are real values so the menu edits them
/// in place; everything else is commented out so future defaults still apply.
const TEMPLATE: &str = r#"# harn settings. Every key is optional: delete or comment out a line to get the default.
# harn-tray applies changes a few seconds after you save this file.
# Reference: https://github.com/SingularityLab/harn#configure

[update]
enabled = true              # "Auto-update agents" in the tray menu
# check_interval_mins = 60

[notify]
enabled = true              # "Notifications" in the tray menu

# [orca]
# bin = "orca"              # path to the Orca CLI if it is not on PATH
# interval_ms = 2000

# [policy]
# settle_ms = 4000          # the screen must be quiet this long before harn acts
# max_actions_per_hour = 40

# [policy.reactions.overloaded]
# action = "continue"       # continue | interrupt | notify | ignore
# delay_secs = 20           # doubles on each attempt...
# max_delay_secs = 600      # ...up to this
# max_attempts = 30
"#;

/// A fresh system often has no app registered for `.toml` or `.log` files.
#[cfg(windows)]
const FALLBACK_EDITOR: Option<&str> = Some("notepad");
#[cfg(target_os = "macos")]
const FALLBACK_EDITOR: Option<&str> = Some("TextEdit");
#[cfg(not(any(windows, target_os = "macos")))]
const FALLBACK_EDITOR: Option<&str> = None;

pub fn open_config(path: &Path) -> Result<()> {
    ensure_config(path)?;
    open_file(path)
}

pub fn open_file(path: &Path) -> Result<()> {
    let Err(error) = open::that_detached(path) else {
        return Ok(());
    };
    let cannot_open = || format!("cannot open {}", path.display());
    match FALLBACK_EDITOR {
        Some(editor) => {
            tracing::warn!(%error, editor, "no default app for {}; trying the fallback", path.display());
            open::with_detached(path, editor).with_context(cannot_open)
        }
        None => Err(error).with_context(cannot_open),
    }
}

fn ensure_config(path: &Path) -> Result<()> {
    if path
        .try_exists()
        .with_context(|| format!("cannot check {}", path.display()))?
    {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    fs::write(path, TEMPLATE).with_context(|| format!("cannot write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_is_a_valid_config_with_default_behaviour_and_is_never_overwritten() {
        let dir = std::env::temp_dir().join(format!("harn-tray-settings-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("config.toml");
        ensure_config(&path).unwrap();
        let cfg = harn::config::load(&path).unwrap();
        let defaults = harn::config::Config::default();
        assert_eq!(cfg.update, defaults.update);
        assert_eq!(cfg.notify, defaults.notify);
        assert_eq!(cfg.policy, defaults.policy);
        harn::config::set_bool(&path, "notify", "enabled", false).unwrap();
        ensure_config(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("enabled = false") && text.contains("# harn settings"),
            "{text}"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
