use jiff::Zoned;

use super::*;

const PROFILE: &str = r#"
id = "test"
name = "Test"
tail_lines = 10
[signatures]
working = ['esc to interrupt']
overloaded = ['overloaded']
server_error = ['API Error: 5\d\d']
network = ['Connection error']
rate_limited = ['rate limited']
usage_limit = ['usage limit reached']
stopped_early = ['(?i)want me to continue\b[^?]*\?\s*$']
user_echo = ['^>(?:\s|$)']
empty_input = ['^>\s*$']
chrome = ['^\s*─{3,}', 'done in \d+s']
reset_hints = ['resets (?P<hour>\d+)(?P<ampm>[ap]m)']
"#;

/// Input box (empty) and footer below the agent's output.
const FOOTER: &str = "─────\n>\n─────\n  ? for shortcuts";

fn profile() -> Profile {
    Profile::from_toml(PROFILE).unwrap()
}

fn msk() -> TimeZone {
    TimeZone::get("Europe/Moscow").unwrap()
}

fn noon() -> u64 {
    let z: Zoned = "2026-10-26T12:00[Europe/Moscow]".parse().unwrap();
    u64::try_from(z.timestamp().as_millisecond()).unwrap()
}

fn run(profile: &Profile, text: &str) -> Classification {
    classify(profile, text, noon(), &msk())
}

fn problem(profile: &Profile, text: &str) -> Problem {
    match run(profile, text) {
        Classification::Problem(p) => p,
        other => panic!("expected a problem, got {other:?} for:\n{text}"),
    }
}

/// `-` for idle, `working`, or the trigger's name.
fn verdict(profile: &Profile, text: &str) -> String {
    match run(profile, text) {
        Classification::Idle => "-".into(),
        Classification::Working => "working".into(),
        Classification::Problem(p) => format!("{:?}", p.trigger),
    }
}

#[test]
fn mechanics() {
    let p = profile();
    let cases = [
        (format!("> hi\nHello!\n{FOOTER}"), "-"),
        (String::new(), "-"),
        // Working anywhere on screen wins, even below an error.
        (
            format!("> go\nAPI Error: 500\n{FOOTER} · esc to interrupt"),
            "working",
        ),
        // Only the last 10 non-blank rows count; the error is the 11th.
        (
            format!("API Error: 500\n\na\nb\n\nc\nd\ne\nf\n{FOOTER}"),
            "-",
        ),
        // Answered by a later user message: history.
        (
            format!("> go\nAPI Error: 500\n> continue\nResumed.\n{FOOTER}"),
            "-",
        ),
        // The input row is never agent output.
        (
            "> go\nAll good.\n─────\n> API Error: 500 typed\n─────".into(),
            "-",
        ),
        // Precedence on one line: usage > rate > overloaded > server > network.
        (
            "API Error: 529 overloaded rate limited usage limit reached".into(),
            "UsageLimit",
        ),
        (
            "API Error: 529 overloaded rate limited Connection error".into(),
            "RateLimited",
        ),
        (
            "API Error: 529 overloaded Connection error".into(),
            "Overloaded",
        ),
        ("API Error: 502 Connection error".into(), "ServerError"),
        // Errors and working beat a question; a question only counts at the very end.
        (
            format!("> go\nWant me to continue?\nConnection error\n{FOOTER}"),
            "Network",
        ),
        (
            format!("> go\nDone. Want me to continue with\n  the rest?\n{FOOTER}"),
            "StoppedEarly",
        ),
        (
            format!("> go\nWant me to continue?\n> yes\nAll done.\n{FOOTER}"),
            "-",
        ),
        (
            format!("> go\nWant me to continue? Let me know.\n{FOOTER}"),
            "-",
        ),
    ];
    for (text, expected) in cases {
        assert_eq!(verdict(&p, &text), expected, "{text}");
    }
}

#[test]
fn problem_details() {
    let p = profile();
    // The last match wins; lines_below stops at the input row, so a taller footer changes nothing.
    let text = format!("> go\n  Connection error  \n> continue\n  API Error: 503 down  \n{FOOTER}");
    let last = problem(&p, &text);
    assert_eq!(
        (last.trigger, last.line.as_str()),
        (Trigger::ServerError, "API Error: 503 down")
    );
    assert_eq!(last.lines_below, 1);
    assert_eq!(
        problem(&p, &format!("{text}\n  status\n  1 shell")).lines_below,
        1
    );
    // Without an input row, counted to the bottom.
    assert_eq!(problem(&p, "API Error: 500 x\na\nb").lines_below, 2);
    let asked = problem(
        &p,
        &format!("> go\nDone. Want me to continue?\ndone in 3s\n{FOOTER}"),
    );
    assert_eq!(
        (asked.trigger, asked.lines_below),
        (Trigger::StoppedEarly, 2)
    );
    // A usage-limit reset may wrap onto the next row.
    let limit = problem(
        &p,
        &format!("> go\nusage limit reached, it\n  resets 3pm\n{FOOTER}"),
    );
    assert_eq!(limit.resets_at_ms, Some(noon() + 3 * 3_600_000));
}

#[test]
fn input_box_state() {
    let p = profile();
    let input = |text: &str| scan(&p, text, noon(), &msk()).input;
    assert_eq!(
        input(&format!("> go\nAPI Error: 500\n{FOOTER}")),
        InputBox::Empty
    );
    let typed = "> go\nAPI Error: 500\n─────\n>   half-typed thought  \n─────";
    assert_eq!(input(typed), InputBox::Draft("half-typed thought".into()));
    // Exited to a shell, or a dialog replaced the prompt: fail closed.
    assert_eq!(input("API Error: 500\nPS C:\\work> "), InputBox::Missing);
    // Without both input-row signature lists the box can never be located.
    for list in [r"empty_input = ['^>\s*$']", r"user_echo = ['^>(?:\s|$)']"] {
        let blind = Profile::from_toml(&PROFILE.replace(list, "")).unwrap();
        let scanned = scan(&blind, "> go\n─────\n>\n─────", noon(), &msk());
        assert_eq!(scanned.input, InputBox::Unknown, "{list}");
    }
    // A draft does not change what the agent is doing.
    let scanned = scan(&p, typed, noon(), &msk());
    assert!(matches!(scanned.class, Classification::Problem(_)));
}

/// `agent | screen line => verdict` for wording from the agents' binaries that no capture shows
/// (real screens are covered by the fixture walker). `-` = must never be a problem: auth,
/// billing, spend caps, warnings, tool output and quotes need a human or are not stops.
const WORDING: &str = "
claude | ● API Error: Repeated 529 Overloaded errors. The API is at capacity. => Overloaded
claude | ● API Error: 529 {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}} => Overloaded
claude | ● Opus is experiencing high load, please use /model to switch to Sonnet => Overloaded
claude | ⏺ API Error: 503 upstream connect error => ServerError
claude | ● API Error: Connection error. => Network
claude | ● API Error: Request rejected (429) · Rate limited => RateLimited
claude | ● API Error: Server is temporarily limiting requests (not your usage limit) => RateLimited
claude | ● You've hit your limit · resets 3pm (Europe/Moscow) => UsageLimit
claude | ● Claude usage limit reached. Your limit will reset at 3pm (Europe/Moscow). => UsageLimit
claude | ● 5-hour limit reached ∙ resets 3pm => UsageLimit
claude | ✻ Session limit reached · Retrying in 1s (11:47pm) · attempt 1/4 => working
claude | · Scampering… => working
claude | * Puzzling… => working
claude | ✻ Newspapering… (2m 39s · ↓ 12.0k tokens) => working
claude | ✢ Compacting conversation… => working
claude | ✻ Baked for 15s · done 3:14 PM => -
claude | ● Let me think… => -
claude |   ⎿  Waiting… => -
claude |   ⎿  API Error: 503 upstream connect error => -
claude |   ⎿  Claude usage limit reached. Your limit will reset at 3pm. => -
claude |   ● API Error: 529 quoted in a user message => -
claude | ● API Error: 401 Invalid API key · Please run /login => -
claude | ● OAuth token revoked · Please run /login => -
claude | ● API Error: 400 Your credit balance is too low to access the Anthropic API. => -
claude | ● You're out of usage credits · resets 5pm (Europe/Moscow) => -
claude | ● You've hit your monthly spend limit · resets Dec 1 => -
claude | ● You've hit your org's monthly spend limit => -
claude | ● You've hit your team's shared budget => -
claude | ● Your org is out of usage · add funds to continue => -
claude | ● API Error: Request was aborted. => -
claude |   You've used 90% of your session limit · resets 5pm (Europe/Moscow) => -
claude |   Approaching weekly limit · resets Nov 2, 5pm => -
codex | ■ exceeded retry limit, last status: 500 Internal Server Error => ServerError
codex | ■ exceeded retry limit, last status: 503 Service Unavailable => ServerError
codex | ■ exceeded retry limit, last status: 529 => Overloaded
codex | • Reconnecting... 2/5 => working
codex | ■ Quota exceeded. Check your plan and billing details. => -
codex | ■ Your workspace is out of credits. Add credits to continue. => -
codex | ■ You hit your spend cap for this month. => -
codex | ■ You’ve hit your usage limit for gpt-5.1-codex. Switch to another model now. => -
codex | ■ Conversation interrupted - use /feedback if something went wrong => -
codex |   └ Stream disconnected before completion: Transport error => -
";

#[test]
fn builtin_wording() {
    let profiles = Profile::builtins();
    for row in WORDING.lines().filter(|row| !row.is_empty()) {
        let (agent, rest) = row.split_once(" | ").unwrap();
        let (line, expected) = rest.rsplit_once(" => ").unwrap();
        let profile = profiles.iter().find(|p| p.id == agent).unwrap();
        assert_eq!(verdict(profile, line), expected, "{row}");
    }
}
