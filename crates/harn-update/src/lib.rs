//! Keep terminal coding agent CLIs on their latest version.
//!
//! [`Updater::ensure_latest`] finds the agent binary, works out how it was installed (npm global,
//! Claude's native installer, Homebrew), compares it with the npm registry's `latest` and runs the
//! matching upgrade command. Expected failures never block a launch: they come back as
//! [`Outcome::Skipped`].

mod cache;
mod detect;
mod system;
mod updater;

pub use updater::Updater;

use serde::Deserialize;

/// An agent CLI harn knows how to update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Agent {
    pub id: &'static str,
    pub binary: &'static str,
    pub npm_package: &'static str,
}

/// All four CLIs are launched by the name they are known by.
const fn agent(id: &'static str, npm_package: &'static str) -> Agent {
    Agent {
        id,
        binary: id,
        npm_package,
    }
}

static AGENTS: [Agent; 4] = [
    agent("claude", "@anthropic-ai/claude-code"),
    agent("codex", "@openai/codex"),
    agent("gemini", "@google/gemini-cli"),
    agent("opencode", "opencode-ai"),
];

const EXECUTABLE_SUFFIXES: [&str; 4] = [".exe", ".cmd", ".bat", ".ps1"];

pub fn known_agents() -> &'static [Agent] {
    &AGENTS
}

/// Looks up an agent by id or binary basename (`claude`, `claude.exe`, `codex`, ...).
pub fn find(name: &str) -> Option<&'static Agent> {
    let file = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .to_ascii_lowercase();
    let stem = EXECUTABLE_SUFFIXES
        .iter()
        .find_map(|ext| file.strip_suffix(ext))
        .unwrap_or(&file);
    AGENTS.iter().find(|a| a.id == stem || a.binary == stem)
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateConfig {
    pub enabled: bool,
    /// How long a looked-up "latest" version is trusted, and the wait before an install that did not
    /// complete (agent still running, command failed) is tried again.
    pub check_interval_mins: u64,
    /// After this long a still-running installer is logged as slow. It is never killed: an
    /// interrupted `npm i -g` can leave the agent half-installed.
    pub install_timeout_secs: u64,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            check_interval_mins: 60,
            install_timeout_secs: 180,
        }
    }
}

/// `Available` is only produced by `check_only`: an update exists but was not installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    UpToDate { version: String },
    Updated { from: String, to: String },
    Available { from: String, to: String },
    Skipped { reason: String },
}

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("`{command}` failed: {tail}")]
    InstallFailed { command: String, tail: String },
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CheckOptions {
    /// Ignore the cached "latest" version and ask the registry, and retry an install even if the
    /// last attempt did not complete.
    pub force: bool,
    /// Report what would happen without installing. Anything that would stop an install (an earlier
    /// attempt that did not complete is still cooling down) is reported as `Skipped` here too.
    pub check_only: bool,
}

#[cfg(test)]
mod testutil {
    //! Temp dirs without `tempfile`, which is not in the workspace lockfile. Removed on drop.
    use std::fs;
    use std::ops::Deref;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    pub(crate) struct TempDir(PathBuf);

    impl TempDir {
        pub(crate) fn new(label: &str) -> Self {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let name = format!("harn-update-{label}-{}-{id}", std::process::id());
            let dir = std::env::temp_dir().join(name);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        /// Writes `rel` (creating parent dirs) and returns its full path.
        pub(crate) fn write(&self, rel: &str, content: &str) -> PathBuf {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, content).unwrap();
            path
        }
    }

    impl Deref for TempDir {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_accepts_ids_basenames_suffixes_and_paths_only_for_known_agents() {
        let windows = r"C:\Users\alice\AppData\Roaming\npm\claude.cmd";
        for (name, id) in [
            ("claude", "claude"),
            ("CLAUDE.EXE", "claude"),
            ("codex.ps1", "codex"),
            ("/usr/local/bin/gemini", "gemini"),
            (windows, "claude"),
        ] {
            assert_eq!(find(name).map(|a| a.id), Some(id), "{name}");
        }
        for name in ["", "bash", "codex.txt", "claude-code", "xcodex"] {
            assert!(find(name).is_none(), "{name}");
        }
        assert_eq!(known_agents().len(), 4);
    }
}
