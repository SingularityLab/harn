//! The agent's command line: this process's exact environment and working directory, plus Windows
//! program resolution that matches what users type in a shell (PATHEXT, npm `.cmd` shims).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use portable_pty::CommandBuilder;

use crate::PtyError;

#[cfg(windows)]
mod windows;

pub(crate) fn build(program: &OsStr, args: &[OsString]) -> Result<CommandBuilder, PtyError> {
    let mut cmd = platform_command(program, args)?;
    // CommandBuilder seeds its own environment (on Windows it re-reads the registry, dropping PATH
    // edits made in the current shell); the agent must see exactly ours.
    cmd.env_clear();
    for (key, value) in std::env::vars_os() {
        cmd.env(key, value);
    }
    let cwd = std::env::current_dir().map_err(|source| PtyError::Io {
        context: "cannot read the current directory",
        source,
    })?;
    // Without an explicit cwd portable-pty starts the child in the home directory.
    cmd.cwd(cwd);
    Ok(cmd)
}

/// True for `dir/agent`, `.\agent.exe` or `/usr/bin/agent`, false for a bare `agent`.
fn has_dir(program: &Path) -> bool {
    program
        .parent()
        .is_some_and(|dir| !dir.as_os_str().is_empty())
}

/// A program given as a path is relative to our cwd, but portable-pty tries a relative path under
/// every PATH entry first and could start a different copy.
fn absolute(program: &Path) -> Result<PathBuf, PtyError> {
    std::path::absolute(program).map_err(|source| PtyError::Io {
        context: "cannot resolve the program path",
        source,
    })
}

#[cfg(not(windows))]
fn platform_command(program: &OsStr, args: &[OsString]) -> Result<CommandBuilder, PtyError> {
    let path = Path::new(program);
    let mut cmd = if has_dir(path) {
        CommandBuilder::new(absolute(path)?)
    } else {
        CommandBuilder::new(program)
    };
    cmd.args(args);
    Ok(cmd)
}

#[cfg(windows)]
fn platform_command(program: &OsStr, args: &[OsString]) -> Result<CommandBuilder, PtyError> {
    let path = std::env::var_os("PATH");
    let pathext = std::env::var_os("PATHEXT");
    let resolved =
        windows::resolve(program, path.as_deref(), pathext.as_deref()).ok_or_else(|| {
            PtyError::NotFound {
                program: program.to_string_lossy().into_owned(),
            }
        })?;
    // Relative PATH entries resolve to relative paths too.
    let resolved = absolute(&resolved)?;
    tracing::debug!(resolved = %resolved.display(), "resolved program");
    if !windows::is_batch(&resolved) {
        let mut cmd = CommandBuilder::new(&resolved);
        cmd.args(args);
        return Ok(cmd);
    }
    windows::check_batch_args(&resolved, args)?;
    // CreateProcess would run a batch file as `cmd.exe /c <line>`, and cmd strips the first and last
    // quote of <line> whenever it starts with one: a quoted script path plus any quoted argument then
    // breaks. Starting the line with `call` keeps every quote intact.
    let mut cmd = CommandBuilder::new(windows::command_interpreter());
    cmd.args(["/d", "/v:off", "/c", "call"]);
    cmd.arg(&resolved);
    cmd.args(args);
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;

    const SHELL: &str = if cfg!(windows) { "cmd" } else { "sh" };

    /// portable-pty tries a relative program under every PATH entry before the current directory,
    /// so `harn run .\claude.exe` could start a different copy.
    #[test]
    fn relative_program_paths_become_absolute() {
        let exe = std::env::current_exe().unwrap();
        let Some(relative) = relative_to_cwd(&exe) else {
            return; // target directory on another drive
        };
        let cmd = build(relative.as_os_str(), &[]).unwrap();
        let argv0 = Path::new(&cmd.get_argv()[0]);
        assert!(argv0.is_absolute(), "{argv0:?}");
        let started = fs::canonicalize(argv0).unwrap();
        assert_eq!(started, fs::canonicalize(&exe).unwrap());
    }

    /// `path` relative to the current directory; `None` across drives.
    fn relative_to_cwd(path: &Path) -> Option<PathBuf> {
        let cwd = std::env::current_dir().unwrap();
        let shared = cwd
            .components()
            .zip(path.components())
            .take_while(|(a, b)| a == b)
            .count();
        if shared == 0 {
            return None;
        }
        let ups = cwd.components().count() - shared;
        let mut relative: PathBuf = std::iter::repeat_n("..", ups).collect();
        relative.extend(path.components().skip(shared));
        Some(relative)
    }

    #[test]
    fn inherits_the_current_directory_and_environment() {
        let cmd = build(OsStr::new(SHELL), &[OsString::from("--flag")]).unwrap();
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(cmd.get_cwd(), Some(&cwd.into_os_string()));
        let (key, value) = std::env::vars_os().next().expect("test env is not empty");
        assert_eq!(cmd.get_env(&key), Some(value.as_os_str()));
    }

    #[test]
    fn unknown_program_is_reported_as_not_found_on_windows() {
        let result = build(OsStr::new("harn-no-such-agent-binary"), &[]);
        if cfg!(windows) {
            assert!(matches!(result, Err(PtyError::NotFound { .. })));
        } else {
            // portable-pty searches PATH at spawn time on Unix.
            assert!(result.is_ok());
        }
    }
}
