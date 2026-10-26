//! Everything that touches the outside world: running programs and asking the npm registry.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use semver::Version;
use serde::Deserialize;

const REGISTRY_URL: &str = "https://registry.npmjs.org";
const REGISTRY_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_OUTPUT_BYTES: u64 = 64 * 1024;

/// A program (a name or path, resolved with `which`) and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cmd {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<String>,
}

impl Cmd {
    pub(crate) fn new(program: impl Into<PathBuf>, args: &[&str]) -> Self {
        let args = args.iter().map(|arg| (*arg).to_owned()).collect();
        Self {
            program: program.into(),
            args,
        }
    }
}

/// `npm i -g @openai/codex@latest`: the program without its directory or `.cmd` suffix.
impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self.program.file_stem().unwrap_or(self.program.as_os_str());
        write!(f, "{}", name.to_string_lossy())?;
        self.args.iter().try_for_each(|arg| write!(f, " {arg}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Limit {
    /// Quick read-only probes: kill the process when the time is up.
    KillAfter(Duration),
    /// Installers are never killed: npm moves the old package aside before it downloads the new
    /// one, so a kill part-way can leave the agent missing. After this long only a warning is logged.
    WarnAfter(Duration),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Output {
    /// Also false for a process that was killed.
    pub(crate) success: bool,
    /// stdout and stderr interleaved; only the last `MAX_OUTPUT_BYTES`.
    pub(crate) text: String,
}

/// What the updater needs from the world; tests fake it.
pub(crate) trait Backend: Send + Sync {
    /// The version the npm registry serves as `latest` for `package`.
    fn latest(&self, package: &str) -> Result<Version, String>;
    /// `ErrorKind::NotFound` when the program is not on PATH.
    fn run(&self, cmd: &Cmd, limit: Limit) -> io::Result<Output>;
}

pub(crate) struct Live;

impl Backend for Live {
    fn latest(&self, package: &str) -> Result<Version, String> {
        fetch_latest(package).map_err(|err| err.to_string())
    }

    fn run(&self, cmd: &Cmd, limit: Limit) -> io::Result<Output> {
        let program = which::which(&cmd.program)
            .map_err(|err| io::Error::new(io::ErrorKind::NotFound, err))?;
        // One command at a time per thread, and the file is removed afterwards, so this is unique.
        let name = format!(
            "harn-update-{}-{:?}.log",
            std::process::id(),
            thread::current().id()
        );
        let log_path = std::env::temp_dir().join(name);
        let output = run_logged(&program, cmd, &log_path, limit);
        // Best effort: it is only a scratch file.
        let _ = fs::remove_file(&log_path);
        output
    }
}

fn fetch_latest(package: &str) -> Result<Version, Box<dyn std::error::Error>> {
    #[derive(Deserialize)]
    struct Manifest {
        version: String,
    }

    let http: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(REGISTRY_TIMEOUT))
        .build()
        .into();
    let manifest: Manifest = http
        .get(format!("{REGISTRY_URL}/{package}/latest"))
        .call()?
        .body_mut()
        .read_json()?;
    Ok(Version::parse(&manifest.version)?)
}

/// Output goes to a scratch file instead of a pipe: there is nothing to drain while we wait, so a
/// chatty installer cannot stall on a full pipe and a grandchild that outlives it cannot hold us up.
fn run_logged(program: &Path, cmd: &Cmd, log_path: &Path, limit: Limit) -> io::Result<Output> {
    let mut log = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(log_path)?;
    let mut command = Command::new(program);
    command
        .args(&cmd.args)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?);
    let mut child = command.spawn()?;
    let status = wait(&mut child, cmd, limit)?;
    Ok(Output {
        success: status.is_some_and(|s| s.success()),
        text: tail(&mut log)?,
    })
}

/// `None`: the process was killed.
fn wait(child: &mut Child, cmd: &Cmd, limit: Limit) -> io::Result<Option<ExitStatus>> {
    let started = Instant::now();
    let mut warned = false;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        match limit {
            Limit::KillAfter(max) if started.elapsed() >= max => {
                child.kill()?;
                child.wait()?;
                return Ok(None);
            }
            Limit::WarnAfter(after) if !warned && started.elapsed() >= after => {
                warned = true;
                tracing::warn!(command = %cmd, secs = after.as_secs(), "still installing; waiting so the install isn't left half-done");
            }
            _ => {}
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn tail(log: &mut File) -> io::Result<String> {
    let len = log.seek(SeekFrom::End(0))?;
    log.seek(SeekFrom::Start(len.saturating_sub(MAX_OUTPUT_BYTES)))?;
    let mut bytes = Vec::new();
    log.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLOW_CHILD: Duration = Duration::from_millis(900);
    const GENEROUS: Duration = Duration::from_secs(60);

    /// Behaves as the child process when `child` re-executes this test binary with a mode argument
    /// (libtest treats it as a filter that matches nothing); does nothing in a normal run.
    #[test]
    fn child_entry() {
        let args: Vec<String> = std::env::args().collect();
        let mode = |name: &str| args.iter().any(|a| a == name);
        if mode("slow") {
            thread::sleep(SLOW_CHILD);
            println!("slow-finished");
        } else if mode("hang") {
            thread::sleep(Duration::from_secs(120));
        } else if mode("noisy") {
            let line = "x".repeat(1023);
            for _ in 0..2048 {
                println!("{line}");
                eprintln!("{line}");
            }
            println!("END-OF-OUTPUT");
        }
    }

    fn child(mode: &str, limit: Limit) -> Output {
        let exe = std::env::current_exe().unwrap();
        let args = ["--exact", "system::tests::child_entry", "--nocapture", mode];
        let cmd = Cmd::new(exe, &args);
        Live.run(&cmd, limit).unwrap()
    }

    #[test]
    fn an_installer_slower_than_its_deadline_is_waited_for_not_killed() {
        let started = Instant::now();
        let out = child("slow", Limit::WarnAfter(Duration::from_millis(100)));
        assert!(out.success && out.text.contains("slow-finished"), "{out:?}");
        assert!(started.elapsed() >= SLOW_CHILD);
    }

    #[test]
    fn a_probe_that_outlives_its_timeout_is_killed() {
        let started = Instant::now();
        let out = child("hang", Limit::KillAfter(Duration::from_millis(400)));
        assert!(!out.success, "{out:?}");
        assert!(started.elapsed() < GENEROUS);
    }

    #[test]
    fn output_is_combined_bounded_to_its_tail_and_never_stalls_the_process() {
        let out = child("noisy", Limit::WarnAfter(GENEROUS));
        assert!(out.success);
        assert!(out.text.contains("END-OF-OUTPUT"));
        assert!(u64::try_from(out.text.len()).unwrap() <= MAX_OUTPUT_BYTES);
    }
}
