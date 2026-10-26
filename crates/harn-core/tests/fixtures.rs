//! Classifies every screen under `tests/fixtures/` and checks it against its filename prefix:
//! `overloaded_529.txt` must be an overloaded problem, `working*.txt` working, and so on. The agent
//! (profile id) is the nearest directory named after a built-in profile, so both
//! `fixtures/claude/x.txt` and `fixtures/synthetic/claude/x.txt` work. Unknown prefixes are skipped.
//! `exited*` and `menu*` screens (agent gone, dialog or picker instead of the prompt) are not
//! classified but left to an engine for two simulated hours: it must never type into them.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns: a failed unwrap is a failed test"
)]

use std::fs;
use std::path::{Path, PathBuf};

use harn_core::{Action, Classification, Engine, Policy, Profile, Snapshot, Trigger, classify};
use jiff::Zoned;
use jiff::tz::TimeZone;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    Idle,
    Working,
    Problem(Trigger),
    /// Old errors still on screen after an echoed `continue` must not count again.
    NotAProblem,
    /// No prompt to type into: whatever the screen shows, the engine must never type.
    NeverTypes,
}

const PREFIXES: [(&str, Expect); 12] = [
    ("idle", Expect::Idle),
    ("working", Expect::Working),
    ("retrying", Expect::Working),
    ("overloaded", Expect::Problem(Trigger::Overloaded)),
    ("server_error", Expect::Problem(Trigger::ServerError)),
    ("network", Expect::Problem(Trigger::Network)),
    ("rate_limited", Expect::Problem(Trigger::RateLimited)),
    ("usage_limit", Expect::Problem(Trigger::UsageLimit)),
    ("stopped_early", Expect::Problem(Trigger::StoppedEarly)),
    ("after_continue", Expect::NotAProblem),
    ("exited", Expect::NeverTypes),
    ("menu", Expect::NeverTypes),
];

/// How long a `NeverTypes` screen is left to the engine, and how often it is re-read.
const NEVER_TYPES_FOR_MS: u64 = 2 * HOUR_MS;
const POLL_MS: u64 = 60_000;

/// Usage-limit fixtures must yield a reset time unless their name says there is none.
const NO_RESET_MARKER: &str = "no_reset";

const MIN_FIXTURES: usize = 30;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "txt") {
            out.push(path);
        }
    }
}

fn profile_for<'a>(path: &Path, profiles: &'a [Profile]) -> Option<&'a Profile> {
    path.ancestors()
        .filter_map(|dir| dir.file_name()?.to_str())
        .find_map(|name| profiles.iter().find(|p| p.id == name))
}

/// The real captures were taken around 20:30 Moscow time on this day.
fn now_ms() -> u64 {
    let z: Zoned = "2026-10-26T20:30[Europe/Moscow]".parse().unwrap();
    u64::try_from(z.timestamp().as_millisecond()).unwrap()
}

fn check(
    text: &str,
    stem: &str,
    profile: &Profile,
    expect: Expect,
    tz: &TimeZone,
) -> Result<(), String> {
    if expect == Expect::NeverTypes {
        return never_types(profile, text, tz);
    }
    let got = classify(profile, text, now_ms(), tz);
    match (expect, &got) {
        (Expect::Idle, Classification::Idle) | (Expect::Working, Classification::Working) => Ok(()),
        (Expect::NotAProblem, c) if !matches!(c, Classification::Problem(_)) => Ok(()),
        (Expect::Problem(t), Classification::Problem(p)) if p.trigger == t => {
            let needs_reset = t == Trigger::UsageLimit && !stem.contains(NO_RESET_MARKER);
            match needs_reset && p.resets_at_ms.is_none() {
                true => Err(format!("no reset time parsed from {:?}", p.line)),
                false => Ok(()),
            }
        }
        _ => Err(format!("expected {expect:?}, got {got:?}")),
    }
}

/// Runs a fresh engine over a screen that stays as it is (long settled), waking at its deadlines
/// and polling every minute, and fails on any keystroke.
fn never_types(profile: &Profile, text: &str, tz: &TimeZone) -> Result<(), String> {
    let mut engine = Engine::with_time_zone(profile.clone(), Policy::default(), tz.clone());
    let start = now_ms();
    let snap = Snapshot {
        text: text.to_owned(),
        last_output_ms: start - HOUR_MS,
    };
    let mut now = start;
    while now <= start + NEVER_TYPES_FOR_MS {
        let typed: Vec<Action> = engine
            .step(&snap, now)
            .into_iter()
            .filter(|a| matches!(a, Action::Input { .. }))
            .collect();
        if !typed.is_empty() {
            return Err(format!(
                "typed after {} s: {typed:?}",
                (now - start) / 1_000
            ));
        }
        let poll = now + POLL_MS;
        now = engine
            .next_deadline_ms()
            .filter(|&at| at > now)
            .map_or(poll, |at| at.min(poll));
    }
    Ok(())
}

#[test]
fn every_fixture_classifies_as_its_name_says() {
    let profiles = Profile::builtins();
    let tz = TimeZone::get("Europe/Moscow").unwrap();
    let mut paths = Vec::new();
    collect(&fixtures_dir(), &mut paths);
    paths.sort();

    let mut checked = 0;
    let mut failures = Vec::new();
    for path in &paths {
        let rel = path.strip_prefix(fixtures_dir()).unwrap_or(path).display();
        let Some(profile) = profile_for(path, &profiles) else {
            failures.push(format!(
                "{rel}: not under a directory named after a profile id"
            ));
            continue;
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let Some(&(_, expect)) = PREFIXES.iter().find(|(prefix, _)| stem.starts_with(prefix))
        else {
            continue;
        };
        checked += 1;
        let text = fs::read_to_string(path).unwrap();
        if let Err(why) = check(&text, stem, profile, expect, &tz) {
            failures.push(format!("{rel}: {why}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} fixture(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        checked >= MIN_FIXTURES,
        "only {checked} fixtures checked; is the walker finding them?"
    );
}

const HOUR_MS: u64 = 3_600_000;
