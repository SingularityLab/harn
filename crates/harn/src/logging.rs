use std::fs::{self, OpenOptions};
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

const LOG_ENV: &str = "HARN_LOG";
pub const DEFAULT_LEVEL: &str = "info";
/// One-shot commands print their own results; logs would only repeat them.
pub const QUIET_LEVEL: &str = "error";
/// Above this the log is truncated at startup instead of growing forever.
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

fn filter(default_level: &str) -> EnvFilter {
    EnvFilter::try_from_env(LOG_ENV).unwrap_or_else(|_| EnvFilter::new(default_level))
}

pub fn to_stderr(default_level: &str) {
    tracing_subscriber::fmt()
        .with_env_filter(filter(default_level))
        .with_writer(std::io::stderr)
        .init();
}

/// For `harn run`: the agent owns the terminal, so anything we print would corrupt its TUI.
pub fn to_file(path: &Path) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let oversized = fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG_BYTES);
    let file = OpenOptions::new()
        .create(true)
        .append(!oversized)
        .write(true)
        .truncate(oversized)
        .open(path)
        .with_context(|| format!("cannot open log file {}", path.display()))?;
    tracing_subscriber::fmt()
        .with_env_filter(filter(DEFAULT_LEVEL))
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .init();
    Ok(())
}
