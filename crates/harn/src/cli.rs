use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "harn",
    version,
    about = "Keep Claude Code, Codex and other terminal agents running: auto-continue on API overload, auto-update on launch"
)]
pub struct Cli {
    /// Config file (default: <config dir>/harn/config.toml).
    #[arg(long, global = true, env = "HARN_CONFIG")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run an agent inside harn's PTY: update it first, then auto-continue it when it stalls.
    Run(RunArgs),
    /// Watch every agent terminal in Orca and auto-continue stalled ones.
    Watch(WatchArgs),
    /// Update agent CLIs to their latest version.
    Update(UpdateArgs),
    /// Classify a saved screen dump (for writing and debugging profiles).
    Check(CheckArgs),
    /// List built-in and user profiles.
    Profiles,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Profile id to use instead of guessing from the program name.
    #[arg(long)]
    pub profile: Option<String>,
    /// Skip the pre-launch update.
    #[arg(long)]
    pub no_update: bool,
    /// Log what would be typed instead of typing it.
    #[arg(long)]
    pub dry_run: bool,
    /// The agent command, e.g. `claude` or `codex --model o3`.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct WatchArgs {
    /// Poll interval (default from config, 2000).
    #[arg(long)]
    pub interval_ms: Option<u64>,
    /// Log what would be typed instead of typing it.
    #[arg(long)]
    pub dry_run: bool,
    /// Don't update agents between sessions.
    #[arg(long)]
    pub no_update: bool,
    /// Watch only this terminal as this agent, e.g. `term_1234=claude` (repeatable). Useful for
    /// agents Orca does not identify. Handles come from `orca terminal list`.
    #[arg(long = "terminal", value_name = "HANDLE=AGENT", value_parser = parse_pin)]
    pub terminals: Vec<harn_orca::TerminalPin>,
}

fn parse_pin(src: &str) -> Result<harn_orca::TerminalPin, String> {
    match src.split_once('=') {
        Some((handle, agent)) if !handle.trim().is_empty() && !agent.trim().is_empty() => {
            Ok(harn_orca::TerminalPin {
                handle: handle.trim().to_owned(),
                agent: agent.trim().to_owned(),
            })
        }
        _ => Err(format!("expected HANDLE=AGENT, got `{src}`")),
    }
}

#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// Agents to update (default: all known agents that are installed).
    pub agents: Vec<String>,
    /// Only report available updates.
    #[arg(long)]
    pub check: bool,
    /// Ignore the cached registry answer.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct CheckArgs {
    /// Profile id.
    #[arg(long, default_value = "claude")]
    pub profile: String,
    /// Screen dump to classify; `-` or nothing reads stdin.
    pub file: Option<PathBuf>,
}
