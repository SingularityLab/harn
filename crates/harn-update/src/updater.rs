//! The update flow: find the binary, work out how it was installed, compare versions, install.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use semver::Version;

use crate::cache::{Cache, MS_PER_MIN, now_ms};
use crate::detect::{InstallMethod, detect, extract_version, package_version};
use crate::system::{Backend, Cmd, Limit, Live};
use crate::{Agent, CheckOptions, Outcome, UpdateConfig, UpdateError};

/// `<bin> --version` is only a fallback, so it must not hold up a launch for long.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const ERROR_TAIL_LINES: usize = 10;
/// Lowercase output of a failed install that means "the files are in use". Codes first: Node prints
/// them verbatim on every OS, while Windows' own text is localised.
const BUSY_MARKERS: [&str; 5] = [
    "ebusy",
    "eperm",
    "resource busy",
    "text file busy",
    "being used by another process",
];

/// Checks agent CLIs against the npm registry and upgrades them. Cheap to build and `Send + Sync`.
pub struct Updater {
    cfg: UpdateConfig,
    cache_dir: PathBuf,
    home: Option<PathBuf>,
    backend: Box<dyn Backend>,
}

/// An installed agent together with the version the registry offers.
struct Target<'a> {
    agent: &'a Agent,
    method: InstallMethod,
    bin: &'a Path,
    installed: Version,
    latest: Version,
}

impl Updater {
    pub fn new(cfg: UpdateConfig, cache_dir: PathBuf) -> Self {
        Self::with_backend(cfg, cache_dir, dirs::home_dir(), Box::new(Live))
    }

    pub(crate) fn with_backend(
        cfg: UpdateConfig,
        cache_dir: PathBuf,
        home: Option<PathBuf>,
        backend: Box<dyn Backend>,
    ) -> Self {
        Self {
            cfg,
            cache_dir,
            home,
            backend,
        }
    }

    /// Bring `agent` to its latest version. Never panics; expected failures (offline, timeout,
    /// binary locked because it is running) come back as `Outcome::Skipped`.
    pub fn ensure_latest(&self, agent: &Agent, opts: CheckOptions) -> Result<Outcome, UpdateError> {
        if !self.cfg.enabled {
            return Ok(skipped("updates are disabled in the config"));
        }
        let Ok(bin) = which::which(agent.binary) else {
            return Ok(skipped(format!("{} is not installed", agent.id)));
        };
        let result = self.update_binary(agent, &bin, opts);
        log_result(agent, &result);
        result
    }

    /// Everything after `which`: split out so tests can point it at a temp-dir layout.
    pub(crate) fn update_binary(
        &self,
        agent: &Agent,
        bin: &Path,
        opts: CheckOptions,
    ) -> Result<Outcome, UpdateError> {
        let Some(method) = detect(agent, bin, self.home.as_deref()) else {
            return Ok(skipped(format!(
                "cannot tell how {} at {} was installed (not npm global, Claude native or Homebrew); update it manually",
                agent.id,
                bin.display()
            )));
        };
        let Some(installed) = self.installed_version(&method, bin) else {
            return Ok(skipped(format!(
                "cannot determine the installed version of {}",
                agent.id
            )));
        };
        let mut cache = Cache::load(&self.cache_dir);
        let latest = match self.latest_version(&mut cache, agent, opts.force) {
            Ok(latest) => latest,
            Err(reason) => return Ok(skipped(reason)),
        };
        if !is_newer(&latest, &installed) {
            return Ok(Outcome::UpToDate {
                version: installed.to_string(),
            });
        }
        // Checked for `check_only` too, so a launch does not announce an update it will then skip.
        let cooldown = (!opts.force)
            .then(|| cache.cooldown_left_ms(agent.npm_package, now_ms(), self.interval_ms()))
            .flatten();
        if let Some(left_ms) = cooldown {
            return Ok(skipped(format!(
                "update {installed} -> {latest} is available, but the last attempt to install it did not complete (cooldown: {} min left)",
                left_ms.div_ceil(MS_PER_MIN)
            )));
        }
        if opts.check_only {
            return Ok(Outcome::Available {
                from: installed.to_string(),
                to: latest.to_string(),
            });
        }
        let target = Target {
            agent,
            method,
            bin,
            installed,
            latest,
        };
        self.install(&target, &mut cache)
    }

    /// From the npm package when there is one (no process spawn), else from `<bin> --version`.
    fn installed_version(&self, method: &InstallMethod, bin: &Path) -> Option<Version> {
        let from_package = match method {
            InstallMethod::Npm { package_dir } => package_version(package_dir),
            _ => None,
        };
        from_package.or_else(|| {
            let probe = Limit::KillAfter(VERSION_PROBE_TIMEOUT);
            let out = self
                .backend
                .run(&Cmd::new(bin, &["--version"]), probe)
                .ok()?;
            out.success.then(|| extract_version(&out.text)).flatten()
        })
    }

    /// Err carries the `Skipped` reason: without a latest version there is nothing to compare.
    fn latest_version(
        &self,
        cache: &mut Cache,
        agent: &Agent,
        force: bool,
    ) -> Result<Version, String> {
        let now = now_ms();
        let cached = if force {
            None
        } else {
            cache.fresh(agent.npm_package, now, self.interval_ms())
        };
        if let Some(latest) = cached {
            return Ok(latest);
        }
        let latest = self
            .backend
            .latest(agent.npm_package)
            .map_err(|err| format!("cannot check the latest {} version: {err}", agent.id))?;
        cache.record_latest(agent.npm_package, &latest, now);
        cache.save(&self.cache_dir);
        Ok(latest)
    }

    fn install(&self, t: &Target, cache: &mut Cache) -> Result<Outcome, UpdateError> {
        let outcome = self.run_install(t);
        let failed_at = if matches!(outcome, Ok(Outcome::Updated { .. })) {
            0
        } else {
            now_ms()
        };
        cache.set_failed(t.agent.npm_package, failed_at);
        cache.save(&self.cache_dir);
        outcome
    }

    fn run_install(&self, t: &Target) -> Result<Outcome, UpdateError> {
        let cmd = t.method.upgrade_cmd(t.agent, t.bin);
        let warn_after = Duration::from_secs(self.cfg.install_timeout_secs);
        tracing::info!(agent = t.agent.id, command = %cmd, from = %t.installed, to = %t.latest, "installing update");
        match self.backend.run(&cmd, Limit::WarnAfter(warn_after)) {
            Ok(out) if out.success => Ok(self.verify(t, &cmd)),
            Ok(out) if is_busy(&out.text) => Ok(skipped(format!(
                "{} is running; will update later",
                t.agent.id
            ))),
            Ok(out) => Err(UpdateError::InstallFailed {
                command: cmd.to_string(),
                tail: tail(&out.text),
            }),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(skipped(format!(
                "cannot update {}: `{}` was not found in PATH",
                t.agent.id,
                cmd.program.display()
            ))),
            Err(source) => Err(UpdateError::Io {
                context: format!("cannot run `{cmd}`"),
                source,
            }),
        }
    }

    /// A zero exit code is not proof: `claude update` and `brew upgrade` exit 0 when their own channel
    /// has nothing newer, and npm may have written to a different prefix.
    fn verify(&self, t: &Target, cmd: &Cmd) -> Outcome {
        let from = t.installed.to_string();
        match self.installed_version(&t.method, t.bin) {
            Some(now) if is_newer(&now, &t.installed) => Outcome::Updated {
                from,
                to: now.to_string(),
            },
            Some(now) => skipped(format!(
                "`{cmd}` succeeded but {} is still at {now} (latest is {})",
                t.agent.id, t.latest
            )),
            None => Outcome::Updated {
                from,
                to: t.latest.to_string(),
            },
        }
    }

    /// How long a looked-up version is trusted, and how long a failed install is left alone.
    fn interval_ms(&self) -> u64 {
        self.cfg.check_interval_mins.saturating_mul(MS_PER_MIN)
    }
}

/// Build metadata never counts, and a pre-release is older than its release.
fn is_newer(candidate: &Version, than: &Version) -> bool {
    candidate.cmp_precedence(than).is_gt()
}

fn is_busy(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    BUSY_MARKERS.iter().any(|marker| text.contains(marker))
}

/// The last lines of the output: where npm and `claude update` put the reason.
fn tail(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    lines[lines.len().saturating_sub(ERROR_TAIL_LINES)..].join(
        "
",
    )
}

fn skipped(reason: impl Into<String>) -> Outcome {
    Outcome::Skipped {
        reason: reason.into(),
    }
}

fn log_result(agent: &Agent, result: &Result<Outcome, UpdateError>) {
    let id = agent.id;
    match result {
        Ok(Outcome::Skipped { reason }) => {
            tracing::warn!(agent = id, %reason, "update skipped")
        }
        Ok(outcome) => tracing::info!(agent = id, ?outcome, "update checked"),
        Err(err) => tracing::warn!(agent = id, error = %err, "update failed"),
    }
}

#[cfg(test)]
mod tests;
