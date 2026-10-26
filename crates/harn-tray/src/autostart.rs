//! "Start at login": Windows Run key, macOS LaunchAgent, Linux XDG autostart entry.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use auto_launch::{AutoLaunch, AutoLaunchBuilder, WindowsEnableMode};

const APP_NAME: &str = "harn-tray";
const FIRST_RUN_MARKER: &str = "tray-autostart-initialized";
const MARKER_NOTE: &str = "harn-tray turned on start at login on its first run. Delete this file to have it do so again.\n";

pub struct Autostart(AutoLaunch);

impl Autostart {
    /// Registers this executable; a custom config is passed along so login starts the same setup.
    pub fn new(config_override: Option<&Path>) -> Result<Self> {
        let exe = std::env::current_exe().context("cannot locate the harn-tray executable")?;
        let mut args = Vec::new();
        if let Some(config) = config_override {
            let config = std::path::absolute(config)
                .with_context(|| format!("cannot resolve {}", config.display()))?;
            args.push("--config".to_owned());
            args.push(command_word(utf8(&config)?));
        }
        let launcher = AutoLaunchBuilder::new()
            .set_app_name(APP_NAME)
            .set_app_path(&command_word(utf8(&exe)?))
            .set_args(&args)
            // Per user: an elevated run must not register the tray for every account.
            .set_windows_enable_mode(WindowsEnableMode::CurrentUser)
            .build()
            .context("start at login is not supported on this system")?;
        Ok(Self(launcher))
    }

    pub fn is_enabled(&self) -> Result<bool> {
        self.0
            .is_enabled()
            .context("cannot read the start-at-login setting")
    }

    pub fn set(&self, enabled: bool) -> Result<()> {
        let changed = if enabled {
            self.0.enable()
        } else {
            self.0.disable()
        };
        changed.context("cannot change the start-at-login setting")
    }
}

/// Start at login is switched on once, on the first run. Afterwards the user's choice stands, so
/// turning it off in the menu is never undone. A failed attempt is retried on the next run.
pub fn on_first_run(marker_dir: &Path, enable: impl FnOnce() -> Result<()>) -> Result<bool> {
    let marker = marker_dir.join(FIRST_RUN_MARKER);
    if marker
        .try_exists()
        .with_context(|| format!("cannot check {}", marker.display()))?
    {
        return Ok(false);
    }
    enable()?;
    fs::create_dir_all(marker_dir)
        .with_context(|| format!("cannot create {}", marker_dir.display()))?;
    fs::write(&marker, MARKER_NOTE)
        .with_context(|| format!("cannot write {}", marker.display()))?;
    Ok(true)
}

fn utf8(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("{} is not valid UTF-8", path.display()))
}

/// The Windows Run key and the XDG `Exec=` line are single command lines, so a path with spaces
/// needs quotes there; macOS gets an argument list and must not have them.
fn command_word(word: &str) -> String {
    if cfg!(target_os = "macos") || !word.contains(' ') {
        word.to_owned()
    } else {
        format!("\"{word}\"")
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use anyhow::bail;

    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("harn-tray-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn enables_once_and_then_respects_the_users_choice() {
        let dir = temp_dir("first-run");
        let calls = Cell::new(0);
        let enable = || {
            calls.set(calls.get() + 1);
            Ok(())
        };
        assert!(on_first_run(&dir, enable).unwrap());
        assert!(!on_first_run(&dir, enable).unwrap());
        assert_eq!(calls.get(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_first_attempt_is_retried_next_run() {
        let dir = temp_dir("first-run-fail");
        assert!(on_first_run(&dir, || bail!("no autostart here")).is_err());
        assert!(on_first_run(&dir, || Ok(())).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn quotes_only_command_lines_that_need_it() {
        assert_eq!(command_word("/usr/bin/harn-tray"), "/usr/bin/harn-tray");
        let spaced = command_word("C:/Users/A B/harn-tray.exe");
        if cfg!(target_os = "macos") {
            assert_eq!(spaced, "C:/Users/A B/harn-tray.exe");
        } else {
            assert_eq!(spaced, "\"C:/Users/A B/harn-tray.exe\"");
        }
    }
}
