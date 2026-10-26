//! Runs a terminal coding agent inside a PTY, passes the terminal through untouched and
//! auto-continues the agent when it stalls.
//!
//! Everything is synchronous: one thread per blocking resource (agent output, agent input, the
//! caller's stdin, the child's exit) and the calling thread running the session loop, which owns the
//! `vt100` screen and the [`harn_core::Engine`]. Nothing is printed to stdout/stderr except the
//! agent's own output; diagnostics go through `tracing`.

use std::ffi::OsString;
use std::io;

use harn_core::{Policy, Profile};

mod automation;
mod command;
mod console;
mod filter;
mod pump;
mod queue;
mod session;

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub program: OsString,
    pub args: Vec<OsString>,
    /// `None` = plain passthrough, no automation (unknown agent).
    pub profile: Option<Profile>,
    pub policy: Policy,
    /// Log actions instead of typing them.
    pub dry_run: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("{0}")]
    Other(String),
    #[error("`{program}` was not found on PATH")]
    NotFound { program: String },
    #[error(
        "cannot pass {arg:?} to {script}: cmd.exe would change it (it contains \", %, ^, a line break \
         or an unquoted &, |, <, > or /?); run the agent's .exe directly or avoid these characters"
    )]
    BatchArgument { script: String, arg: String },
    #[error("cannot prepare the terminal: {0}")]
    Console(#[source] io::Error),
    #[error("cannot start `{program}`: {reason}")]
    Spawn { program: String, reason: String },
    #[error("{context}: {reason}")]
    Pty {
        context: &'static str,
        reason: String,
    },
    #[error("{context}: {source}")]
    Io {
        context: &'static str,
        #[source]
        source: io::Error,
    },
}

/// Blocks until the agent exits and returns its exit code. The caller's terminal is restored
/// before returning, also on error.
///
/// The thread reading the caller's stdin cannot be interrupted portably and may still be blocked in
/// `read` when this returns; exit the process right after (otherwise it may swallow one keystroke).
pub fn run(opts: RunOptions) -> Result<i32, PtyError> {
    let RunOptions {
        program,
        args,
        profile,
        policy,
        dry_run,
    } = opts;
    let command = command::build(&program, &args)?;
    let size = console::terminal_size().unwrap_or_default();
    tracing::info!(
        program = %program.to_string_lossy(),
        ?args,
        profile = profile.as_ref().map(|p| p.id.as_str()),
        dry_run,
        rows = size.rows,
        cols = size.cols,
        "starting agent"
    );
    let _console = console::ConsoleGuard::enable()?;
    let pty = session::Pty::spawn(command, size, &program)?;
    let automation = profile.map(|profile| automation::Automation {
        profile,
        policy,
        dry_run,
    });
    let code = session::run(pty, automation, console::stdio_terminal())?;
    tracing::info!(code, "agent exited");
    Ok(code)
}
