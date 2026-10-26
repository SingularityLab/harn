use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, io};

use anyhow::{Context, Result, bail};
use harn_core::{Action, Profile, classify};
use harn_update::{CheckOptions, Outcome, Updater};

use crate::cli::{CheckArgs, RunArgs, UpdateArgs};
use harn::config::{Config, LoadedProfile, Paths};
use harn::notify::{self, Notifier};
use harn::updates::describe;

pub fn run(args: RunArgs, cfg: Config, paths: &Paths, profiles: &[LoadedProfile]) -> Result<i32> {
    let mut command = args.command.into_iter();
    let program = command.next().context("missing agent command")?;
    let name = program_name(&program);
    let profile = match &args.profile {
        Some(id) => Some(find_profile(profiles, id)?.clone()),
        None => profiles
            .iter()
            .find(|p| p.profile.matches(&name))
            .map(|p| p.profile.clone()),
    };
    if profile.is_none() {
        tracing::warn!(program = %name, "no profile matches; running as plain passthrough");
    }
    if cfg.update.enabled && !args.no_update {
        update_before_launch(&name, &cfg, paths);
    }
    tracing::info!(program = %name, profile = ?profile.as_ref().map(|p| &p.id), "launching");
    let actions = profile
        .as_ref()
        .and_then(|p| spawn_notifier(Notifier::new(&cfg.notify), p.id.clone()));
    let opts = harn_pty::RunOptions {
        program,
        args: command.collect(),
        profile,
        policy: cfg.policy,
        dry_run: args.dry_run,
        actions,
    };
    harn_pty::run(opts).context("agent session failed")
}

/// Shows a desktop notification for what the session does. The thread ends when the session drops
/// the sender; if it cannot start, the agent launches without notifications.
fn spawn_notifier(notifier: Notifier, agent: String) -> Option<Sender<Action>> {
    let (actions, received) = mpsc::channel();
    let notify_all = move || {
        for action in received {
            if let Some(note) = notify::for_action(&agent, &action) {
                notifier.send(note);
            }
        }
    };
    match thread::Builder::new()
        .name("harn-notify".into())
        .spawn(notify_all)
    {
        Ok(_) => Some(actions),
        Err(error) => {
            tracing::warn!(%error, "cannot start the notification thread; notifications are off");
            None
        }
    }
}

/// Runs before the agent owns the terminal, so progress may go to stderr. Never blocks the launch on failure.
fn update_before_launch(name: &str, cfg: &Config, paths: &Paths) {
    let Some(agent) = harn_update::find(name) else {
        return;
    };
    let updater = Updater::new(cfg.update.clone(), paths.cache_dir.clone());
    let check = CheckOptions {
        force: false,
        check_only: true,
    };
    match updater.ensure_latest(agent, check) {
        Ok(Outcome::Available { from, to }) => {
            eprintln!(
                "harn: updating {} {from} -> {to} (this can take a minute)...",
                agent.id
            );
            match updater.ensure_latest(agent, CheckOptions::default()) {
                Ok(outcome) => eprintln!("harn: {}", describe(agent.id, &outcome)),
                Err(e) => eprintln!(
                    "harn: update of {} failed, launching current version: {e}",
                    agent.id
                ),
            }
        }
        Ok(outcome) => tracing::info!(agent = agent.id, "{}", describe(agent.id, &outcome)),
        Err(e) => tracing::warn!(agent = agent.id, error = %e, "update check failed"),
    }
}

pub fn update(args: UpdateArgs, cfg: Config, paths: &Paths) -> Result<i32> {
    let agents = if args.agents.is_empty() {
        harn_update::known_agents().iter().collect::<Vec<_>>()
    } else {
        args.agents
            .iter()
            .map(|name| harn_update::find(name).with_context(|| format!("unknown agent `{name}`")))
            .collect::<Result<Vec<_>>>()?
    };
    let updater = Updater::new(cfg.update, paths.cache_dir.clone());
    let opts = CheckOptions {
        force: args.force,
        check_only: args.check,
    };
    let mut failed = false;
    for agent in agents {
        match updater.ensure_latest(agent, opts) {
            Ok(outcome) => println!("{}", describe(agent.id, &outcome)),
            Err(e) => {
                failed = true;
                eprintln!("{}: update failed: {e}", agent.id);
            }
        }
    }
    Ok(i32::from(failed))
}

pub fn check(args: CheckArgs, profiles: &[LoadedProfile]) -> Result<i32> {
    let profile = find_profile(profiles, &args.profile)?;
    let text = match args.file.as_deref() {
        Some(path) if path != Path::new("-") => {
            fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?
        }
        _ => {
            let mut buf = String::new();
            io::stdin()
                .read_to_string(&mut buf)
                .context("cannot read stdin")?;
            buf
        }
    };
    let classification = classify(profile, &text, now_ms(), &jiff::tz::TimeZone::system());
    println!("{classification:#?}");
    Ok(0)
}

pub fn list_profiles(profiles: &[LoadedProfile]) -> Result<i32> {
    for loaded in profiles {
        let p = &loaded.profile;
        let source = loaded
            .source
            .as_ref()
            .map_or_else(|| "built-in".to_owned(), |s| s.display().to_string());
        println!(
            "{:<10} {:<14} aliases: {:<28} {source}",
            p.id,
            p.name,
            p.aliases.join(",")
        );
    }
    Ok(0)
}

fn find_profile<'a>(profiles: &'a [LoadedProfile], id: &str) -> Result<&'a Profile> {
    match profiles.iter().find(|p| p.profile.id == id) {
        Some(p) => Ok(&p.profile),
        None => {
            let known: Vec<_> = profiles.iter().map(|p| p.profile.id.as_str()).collect();
            bail!("unknown profile `{id}` (known: {})", known.join(", "))
        }
    }
}

fn program_name(program: &OsStr) -> String {
    Path::new(program)
        .file_name()
        .unwrap_or(program)
        .to_string_lossy()
        .into_owned()
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
