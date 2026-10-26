use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::{fs, io};

use super::*;
use crate::system::Output;
use crate::testutil::TempDir;
use crate::{agent, find, known_agents};

const fn opts(force: bool, check_only: bool) -> CheckOptions {
    CheckOptions { force, check_only }
}
const INSTALL: CheckOptions = opts(false, false);
const CHECK: CheckOptions = opts(false, true);
const FORCE: CheckOptions = opts(true, false);
const CODEX_PACKAGE: &str = "@openai/codex";

type RunFn = Arc<dyn Fn(&Cmd, Limit) -> io::Result<Output> + Send + Sync>;

/// Stands in for the registry and the process runner; clones share what was asked of it.
#[derive(Clone)]
struct Fake {
    latest: Result<&'static str, &'static str>,
    run: RunFn,
    lookups: Arc<AtomicUsize>,
    commands: Arc<Mutex<Vec<String>>>,
    limits: Arc<Mutex<Vec<Limit>>>,
}

impl Fake {
    fn new(latest: Result<&'static str, &'static str>, run: RunFn) -> Self {
        let (lookups, commands, limits) = Default::default();
        Self {
            latest,
            run,
            lookups,
            commands,
            limits,
        }
    }

    /// Nothing may be run.
    fn idle(latest: &'static str) -> Self {
        Self::new(
            Ok(latest),
            Arc::new(|cmd, _| panic!("unexpected command: {cmd}")),
        )
    }

    /// Every command ends with `output`.
    fn replying(latest: &'static str, output: Output) -> Self {
        Self::new(Ok(latest), Arc::new(move |_, _| Ok(output.clone())))
    }

    fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }

    fn limit(&self, call: usize) -> Limit {
        self.limits.lock().unwrap()[call]
    }
}

impl Backend for Fake {
    fn latest(&self, _package: &str) -> Result<Version, String> {
        self.lookups.fetch_add(1, SeqCst);
        self.latest
            .map(|v| Version::parse(v).unwrap())
            .map_err(str::to_owned)
    }

    fn run(&self, cmd: &Cmd, limit: Limit) -> io::Result<Output> {
        self.commands.lock().unwrap().push(cmd.to_string());
        self.limits.lock().unwrap().push(limit);
        (self.run)(cmd, limit)
    }
}

fn out(success: bool, text: &str) -> Output {
    let text = text.to_owned();
    Output { success, text }
}

fn updated(from: &str, to: &str) -> Outcome {
    let (from, to) = (from.into(), to.into());
    Outcome::Updated { from, to }
}

fn skip(result: Result<Outcome, UpdateError>) -> String {
    match result {
        Ok(Outcome::Skipped { reason }) => reason,
        other => panic!("expected Skipped, got {other:?}"),
    }
}

/// Every part must appear in `text`.
fn mentions(text: &str, parts: &[&str]) {
    for part in parts {
        assert!(text.contains(part), "{part:?} is missing from {text:?}");
    }
}

fn updater(cfg: UpdateConfig, dir: &TempDir, fake: &Fake) -> Updater {
    let (cache, home) = (dir.join("cache"), Some(dir.join("home")));
    Updater::with_backend(cfg, cache, home, Box::new(fake.clone()))
}

/// An npm-global install in the Windows shape (`<prefix>\<bin>.cmd` + `<prefix>\node_modules\<pkg>`).
struct Fx {
    root: TempDir,
    agent: &'static Agent,
    pkg_json: PathBuf,
}

impl Fx {
    fn new(agent: &str, installed: &str) -> Self {
        let agent = find(agent).unwrap();
        let root = TempDir::new("flow");
        root.write(&format!("npm/{}.cmd", agent.binary), "");
        let pkg = format!("npm/node_modules/{}/package.json", agent.npm_package);
        let pkg_json = root.write(&pkg, &format!(r#"{{"version":"{installed}"}}"#));
        Self {
            root,
            agent,
            pkg_json,
        }
    }

    fn bin(&self) -> PathBuf {
        self.root.join(format!("npm/{}.cmd", self.agent.binary))
    }

    fn up(&self, fake: &Fake) -> Updater {
        updater(UpdateConfig::default(), &self.root, fake)
    }

    fn go(&self, up: &Updater, opts: CheckOptions) -> Result<Outcome, UpdateError> {
        up.update_binary(self.agent, &self.bin(), opts)
    }

    fn run(&self, fake: &Fake, opts: CheckOptions) -> Result<Outcome, UpdateError> {
        self.go(&self.up(fake), opts)
    }

    /// A backend whose commands "install" `version` by rewriting package.json, like npm would.
    fn installing(&self, latest: &'static str, version: &'static str) -> Fake {
        let path = self.pkg_json.clone();
        let run: RunFn = Arc::new(move |_, _| {
            fs::write(&path, format!(r#"{{"version":"{version}"}}"#)).unwrap();
            Ok(out(true, "added 1 package"))
        });
        Fake::new(Ok(latest), run)
    }
}

#[test]
fn disabled_config_missing_binary_and_unknown_install_are_skipped_without_touching_anything() {
    let dir = TempDir::new("skip");
    let fake = Fake::idle("1.0.0");
    let codex = find("codex").unwrap();

    let off = UpdateConfig {
        enabled: false,
        ..UpdateConfig::default()
    };
    let reason = skip(updater(off, &dir, &fake).ensure_latest(codex, INSTALL));
    mentions(&reason, &["disabled"]);

    let up = updater(UpdateConfig::default(), &dir, &fake);
    let ghost = agent("harn-update-no-such-binary", "ghost");
    mentions(
        &skip(up.ensure_latest(&ghost, INSTALL)),
        &["is not installed"],
    );

    let bin = dir.write("tools/codex.exe", "");
    let reason = skip(up.update_binary(codex, &bin, INSTALL));
    mentions(&reason, &["cannot tell how codex", "codex.exe"]);
    assert_eq!(fake.lookups.load(SeqCst), 0);
}

#[test]
fn versions_are_compared_with_semver_including_prereleases_and_check_only_installs_nothing() {
    for (installed, latest, available) in [
        ("0.164.0", "0.164.0", false),
        ("0.165.0-alpha.2", "0.164.0", false),
        ("0.163.0", "0.164.0", true),
        ("0.165.0-alpha.2", "0.165.0", true),
        ("0.165.0-alpha.2", "0.165.0-alpha.10", true),
    ] {
        let fake = Fake::idle(latest);
        let got = Fx::new("codex", installed).run(&fake, CHECK).unwrap();
        let (from, to, version) = (installed.into(), latest.into(), installed.into());
        let want = match available {
            true => Outcome::Available { from, to },
            false => Outcome::UpToDate { version },
        };
        assert_eq!(got, want, "{installed} vs {latest}");
    }
}

#[test]
fn npm_install_runs_npm_with_a_warning_deadline_and_reports_the_version_read_back() {
    let fx = Fx::new("codex", "0.163.0");
    let fake = fx.installing("0.164.0", "0.164.0");
    let cfg = UpdateConfig {
        install_timeout_secs: 42,
        ..UpdateConfig::default()
    };

    let got = fx.go(&updater(cfg, &fx.root, &fake), INSTALL).unwrap();

    assert_eq!(got, updated("0.163.0", "0.164.0"));
    assert_eq!(fake.commands(), ["npm i -g @openai/codex@latest"]);
    assert_eq!(fake.limit(0), Limit::WarnAfter(Duration::from_secs(42)));
}

#[test]
fn claude_native_install_reads_its_version_from_the_binary_and_updates_itself() {
    let root = TempDir::new("native");
    let bin = root.write("home/.local/bin/claude.exe", "");
    // Popped from the end: the version probe, the update, the version probe after it.
    let replies = Mutex::new(vec!["2.1.310", "ok", "2.1.300"]);
    let run: RunFn = Arc::new(move |_, _| Ok(out(true, replies.lock().unwrap().pop().unwrap())));
    let fake = Fake::new(Ok("2.1.310"), run);
    let up = updater(UpdateConfig::default(), &root, &fake);
    let claude = find("claude").unwrap();

    let got = up.update_binary(claude, &bin, INSTALL).unwrap();

    assert_eq!(got, updated("2.1.300", "2.1.310"));
    assert_eq!(
        fake.commands(),
        ["claude --version", "claude update", "claude --version"]
    );
    assert_eq!(fake.limit(0), Limit::KillAfter(VERSION_PROBE_TIMEOUT));
}

#[test]
fn a_locked_install_is_skipped_as_running_and_cools_down_until_forced_or_expired() {
    let fx = Fx::new("codex", "0.163.0");
    let fake = Fake::replying("0.164.0", out(false, "npm error code EBUSY"));
    let up = fx.up(&fake);

    assert_eq!(
        skip(fx.go(&up, INSTALL)),
        "codex is running; will update later"
    );
    for mode in [INSTALL, CHECK] {
        let reason = skip(fx.go(&up, mode));
        mentions(&reason, &["0.163.0 -> 0.164.0 is available", "cooldown"]);
    }
    assert_eq!(
        fake.commands().len(),
        1,
        "no second install while cooling down"
    );
    skip(fx.go(&up, FORCE));
    assert_eq!(fake.commands().len(), 2, "force retries at once");

    let dir = fx.root.join("cache");
    let mut cache = Cache::load(&dir);
    cache.set_failed(CODEX_PACKAGE, 1);
    cache.save(&dir);
    let got = fx.go(&up, CHECK).unwrap();
    assert!(
        matches!(got, Outcome::Available { .. }),
        "an old failure is over: {got:?}"
    );
}

#[test]
fn install_failures_become_errors_or_skips_and_a_zero_exit_without_a_new_version_is_not_an_update()
{
    let fx = Fx::new("codex", "0.163.0");
    let log = "npm error code E404\nnpm error 404 Not Found - GET https://registry.npmjs.org/x";
    let failing = Fake::replying("0.164.0", out(false, log));
    let error = fx.run(&failing, INSTALL).unwrap_err().to_string();
    mentions(&error, &["npm i -g @openai/codex@latest", "404 Not Found"]);

    let no_npm: RunFn = Arc::new(|_, _| Err(ErrorKind::NotFound.into()));
    let reason = skip(fx.run(&Fake::new(Ok("0.164.0"), no_npm), FORCE));
    mentions(&reason, &["`npm` was not found in PATH"]);

    let unchanged = Fake::replying("0.164.0", out(true, "up to date"));
    let reason = skip(fx.run(&unchanged, FORCE));
    mentions(&reason, &["codex is still at 0.163.0", "latest is 0.164.0"]);

    let offline = Fake::new(
        Err("timeout"),
        Arc::new(|cmd, _| panic!("unexpected: {cmd}")),
    );
    let reason = skip(fx.run(&offline, FORCE));
    mentions(&reason, &["cannot check the latest codex version: timeout"]);
}

#[test]
fn latest_is_cached_for_the_interval_force_bypasses_it_and_a_corrupt_cache_is_empty() {
    let fx = Fx::new("codex", "0.163.0");
    let fake = Fake::idle("0.164.0");
    let up = fx.up(&fake);
    let lookups = || fake.lookups.load(SeqCst);

    fx.go(&up, CHECK).unwrap();
    fx.go(&up, CHECK).unwrap();
    assert_eq!(lookups(), 1, "second check is served from the cache");
    fx.go(&up, opts(true, true)).unwrap();
    assert_eq!(lookups(), 2, "force asks the registry again");

    let cache_file = fx.root.join("cache/update-cache.json");
    fs::write(&cache_file, "{ not json").unwrap();
    fx.go(&up, CHECK).unwrap();
    assert_eq!(lookups(), 3, "a corrupt cache is ignored");
    let rewritten = fs::read_to_string(&cache_file).unwrap();
    mentions(&rewritten, &[CODEX_PACKAGE, "0.164.0"]);
}

/// Real registry and this machine's agents, always check-only. `-- --ignored --nocapture` to run.
#[test]
#[ignore = "needs network and inspects this machine's agents"]
fn real_registry_and_installed_agents_in_check_only_mode() {
    let scratch = TempDir::new("real");
    let (cache, backend) = (scratch.join("cache"), Box::new(Live));
    let up = Updater::with_backend(UpdateConfig::default(), cache, dirs::home_dir(), backend);
    for agent in known_agents() {
        let latest = Live.latest(agent.npm_package).unwrap();
        let outcome = up.ensure_latest(agent, opts(true, true)).unwrap();
        println!("{:<9} latest {latest:<9} {outcome:?}", agent.id);
        assert!(!matches!(outcome, Outcome::Updated { .. }));
    }
}
