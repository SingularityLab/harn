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

/// Input box (empty) and footer: 4 rows below the agent's output.
const FOOTER: &str = "─────\n>\n─────\n  ? for shortcuts";

fn profile() -> Profile {
    Profile::from_toml(PROFILE).unwrap()
}

fn builtin(id: &str) -> Profile {
    Profile::builtins()
        .into_iter()
        .find(|p| p.id == id)
        .unwrap()
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

fn trigger(profile: &Profile, text: &str) -> Option<Trigger> {
    match run(profile, text) {
        Classification::Problem(p) => Some(p.trigger),
        _ => None,
    }
}

#[test]
fn idle_when_nothing_matches() {
    assert_eq!(
        run(&profile(), &format!("> hi\n\nHello!\n{FOOTER}")),
        Classification::Idle
    );
    assert_eq!(run(&profile(), ""), Classification::Idle);
}

#[test]
fn working_anywhere_wins_over_problems() {
    let text = format!("> go\nAPI Error: 500 boom\n{FOOTER} · esc to interrupt");
    assert_eq!(run(&profile(), &text), Classification::Working);
}

#[test]
fn only_the_tail_counts_and_blank_lines_are_not_lines() {
    let p = profile();
    // 10 non-blank rows at the bottom; the error is the 11th from the bottom.
    let text = format!("API Error: 500 old\n\na\nb\n\nc\nd\ne\nf\n{FOOTER}");
    assert_eq!(run(&p, &text), Classification::Idle);
    let text = format!("API Error: 500 recent\n\na\nb\n\nc\nd\ne\n{FOOTER}");
    assert_eq!(problem(&p, &text).lines_below, 6);
}

#[test]
fn errors_answered_by_a_later_user_message_do_not_count() {
    let p = profile();
    let resumed =
        format!("> go\nAPI Error: 500 boom\ndone in 3s\n> continue\nResumed fine.\n{FOOTER}");
    assert_eq!(run(&p, &resumed), Classification::Idle);
    let failed_again =
        format!("> go\nAPI Error: 500 boom\n> continue\nAPI Error: 503 again\n{FOOTER}");
    let again = problem(&p, &failed_again);
    assert_eq!(again.line, "API Error: 503 again");
    assert_eq!(again.lines_below, 1);
}

#[test]
fn the_input_row_is_never_output() {
    let text = "> go\nAll good.\n─────\n> API Error: 500 typed by the user\n─────";
    assert_eq!(run(&profile(), text), Classification::Idle);
}

#[test]
fn input_box_state_is_reported() {
    let p = profile();
    let input = |text: &str| scan(&p, text, noon(), &msk()).input;
    let empty = format!("> go\nAPI Error: 500\n{FOOTER}");
    assert_eq!(input(&empty), InputBox::Empty);
    let typed = "> go\nAPI Error: 500\n─────\n>   half-typed thought  \n─────";
    assert_eq!(input(typed), InputBox::Draft("half-typed thought".into()));
    // Exited to a shell, or a dialog replaced the prompt: fail closed.
    assert_eq!(input("API Error: 500\nPS C:\\work> "), InputBox::Missing);
    // Without both input-row signature lists the box can never be located.
    for list in [r"empty_input = ['^>\s*$']", r"user_echo = ['^>(?:\s|$)']"] {
        let blind = Profile::from_toml(&PROFILE.replace(list, "")).unwrap();
        assert!(!blind.signatures.locates_input(), "{list}");
        let scanned = scan(&blind, "> go\n─────\n>\n─────", noon(), &msk());
        assert_eq!(scanned.input, InputBox::Unknown, "{list}");
    }
    let claude = builtin("claude");
    let swallowed = scan(&claude, "❯ continue\n──────", noon(), &msk()).input;
    assert_eq!(swallowed.draft(), Some("continue"));
    let codex = builtin("codex");
    let placeholder = scan(
        &codex,
        "› say hi\n› Ask Codex to do anything",
        noon(),
        &msk(),
    );
    assert_eq!(placeholder.input, InputBox::Empty);
    // A draft does not change what the agent is doing.
    let scanned = scan(
        &p,
        "> go\nAPI Error: 500\n─────\n> draft\n─────",
        noon(),
        &msk(),
    );
    assert!(matches!(scanned.class, Classification::Problem(_)));
}

#[test]
fn lines_below_stop_at_the_input_row() {
    let p = profile();
    let short = problem(&p, &format!("> go\nAPI Error: 500\n{FOOTER}"));
    let tall_footer = format!("> go\nAPI Error: 500\n{FOOTER}\n  status line\n  1 shell");
    assert_eq!(short.lines_below, 1);
    assert_eq!(problem(&p, &tall_footer).lines_below, 1);
}

#[test]
fn without_echo_signatures_the_whole_tail_is_the_turn() {
    let src = PROFILE.replace("user_echo = ['^>(?:\\s|$)']", "");
    let p = Profile::from_toml(&src).unwrap();
    let text = format!("> go\nAPI Error: 500 boom\n> continue\nResumed fine.\n{FOOTER}");
    assert_eq!(trigger(&p, &text), Some(Trigger::ServerError));
}

#[test]
fn last_matching_line_wins() {
    let text =
        format!("> go\n  Connection error  \nretrying by hand\n  API Error: 503 down  \n{FOOTER}");
    let p = problem(&profile(), &text);
    assert_eq!(p.trigger, Trigger::ServerError);
    assert_eq!(p.line, "API Error: 503 down");
    assert_eq!(p.lines_below, 1);
    assert_eq!(p.resets_at_ms, None);
}

#[test]
fn precedence_within_one_line() {
    let p = profile();
    let cases = [
        (
            "API Error: 529 overloaded rate limited usage limit reached",
            Trigger::UsageLimit,
        ),
        (
            "API Error: 529 overloaded rate limited Connection error",
            Trigger::RateLimited,
        ),
        (
            "API Error: 529 overloaded Connection error",
            Trigger::Overloaded,
        ),
        ("API Error: 502 Connection error", Trigger::ServerError),
        ("Connection error", Trigger::Network),
    ];
    for (line, expected) in cases {
        assert_eq!(trigger(&p, line), Some(expected), "{line}");
    }
}

#[test]
fn usage_limit_reset_time_may_wrap_onto_the_next_line() {
    let p = problem(
        &profile(),
        &format!("> go\nusage limit reached, it\n  resets 3pm\n{FOOTER}"),
    );
    assert_eq!(p.trigger, Trigger::UsageLimit);
    assert_eq!(p.resets_at_ms, Some(noon() + 3 * 3_600_000));
    let p = problem(&profile(), &format!("> go\nusage limit reached\n{FOOTER}"));
    assert_eq!(p.resets_at_ms, None);
}

#[test]
fn stopped_early_only_at_the_end_of_the_output() {
    let p = profile();
    let asked = problem(
        &p,
        &format!("> do it\nDone part one. Want me to continue?\ndone in 3s\n{FOOTER}"),
    );
    assert_eq!(asked.trigger, Trigger::StoppedEarly);
    assert_eq!(asked.line, "Done part one. Want me to continue?");
    assert_eq!(asked.lines_below, 2);
    assert_eq!(asked.resets_at_ms, None);

    let wrapped =
        format!("> do it\nDone. Want me to continue with\n  the remaining files?\n{FOOTER}");
    assert_eq!(trigger(&p, &wrapped), Some(Trigger::StoppedEarly));

    let answered = format!("> do it\nWant me to continue?\n> yes\nAll done.\n{FOOTER}");
    assert_eq!(run(&p, &answered), Classification::Idle);
    let trailing = format!("> do it\nWant me to continue? Let me know.\n{FOOTER}");
    assert_eq!(run(&p, &trailing), Classification::Idle);
}

#[test]
fn errors_and_working_beat_stopped_early() {
    let p = profile();
    let text = format!("> go\nWant me to continue?\nConnection error\n{FOOTER}");
    assert_eq!(trigger(&p, &text), Some(Trigger::Network));
    let text = format!("> go\nWant me to continue?\n{FOOTER} esc to interrupt");
    assert_eq!(run(&p, &text), Classification::Working);
}

#[test]
fn claude_strings() {
    let claude = builtin("claude");
    let cases = [
        (
            "● API Error: Repeated 529 Overloaded errors. The API is at capacity — this is usually temporary.",
            Trigger::Overloaded,
        ),
        (
            "● Opus is experiencing high load, please use /model to switch to Sonnet",
            Trigger::Overloaded,
        ),
        (
            r#"● API Error: 529 {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            Trigger::Overloaded,
        ),
        (
            "● API Error: 500 Internal server error. This is a server-side issue",
            Trigger::ServerError,
        ),
        (
            "● API Error: 503 upstream connect error",
            Trigger::ServerError,
        ),
        (
            "● API Error: Connection dropped (ECONNRESET)",
            Trigger::Network,
        ),
        ("● API Error: Connection error.", Trigger::Network),
        (
            "● API Error: Request timed out. Check your internet connection",
            Trigger::Network,
        ),
        (
            "● API Error: Request rejected (429) · Rate limited",
            Trigger::RateLimited,
        ),
        (
            "● You've hit your session limit · resets 11:47pm (Europe/Moscow)",
            Trigger::UsageLimit,
        ),
        (
            "● You've hit your weekly limit · resets Nov 2, 5pm (Europe/Moscow)",
            Trigger::UsageLimit,
        ),
        (
            "● You've hit your limit · resets 3pm (Europe/Moscow)",
            Trigger::UsageLimit,
        ),
        (
            "● Claude usage limit reached. Your limit will reset at 3pm (Europe/Moscow).",
            Trigger::UsageLimit,
        ),
        ("● 5-hour limit reached ∙ resets 3pm", Trigger::UsageLimit),
    ];
    for (line, expected) in cases {
        assert_eq!(trigger(&claude, line), Some(expected), "{line}");
    }
    let session = problem(
        &claude,
        "● You've hit your session limit · resets 11:47pm (Europe/Moscow)",
    );
    assert_eq!(
        session.resets_at_ms,
        Some(noon() + 11 * 3_600_000 + 47 * 60_000)
    );
}

#[test]
fn claude_never_continues_on_errors_that_need_a_human() {
    let claude = builtin("claude");
    for line in [
        // "⎿" rows are tool output in 2.x: an API error printed by a command Claude ran.
        "  ⎿  API Error: 503 upstream connect error",
        r#"  ⎿  API Error: 529 {"type":"error","error":{"type":"overloaded_error"}}"#,
        "  ⎿  Claude usage limit reached. Your limit will reset at 3pm (Europe/Moscow).",
        "  ⎿  5-hour limit reached ∙ resets 3pm",
        "● API Error: 401 Invalid API key · Please run /login",
        "● OAuth token revoked · Please run /login",
        "● Login expired · Please run /login",
        "● API Error: 400 Your credit balance is too low to access the Anthropic API.",
        "● You're out of usage credits · resets 5pm (Europe/Moscow)",
        "● You're out of extra usage",
        "● You've hit your monthly spend limit · resets Dec 1",
        "● You've hit your org's monthly spend limit",
        "● You've hit your team's shared budget",
        "● Your org is out of usage · add funds to continue",
        "● API Error: Request was aborted.",
        "  You've used 90% of your session limit · resets 5pm (Europe/Moscow)",
        "  Approaching weekly limit · resets Nov 2, 5pm",
    ] {
        assert_eq!(run(&claude, line), Classification::Idle, "{line}");
    }
    let banner = "✻ Session limit reached · Retrying in 1s (11:47pm) · attempt 1/4";
    assert_eq!(run(&claude, banner), Classification::Working);
}

#[test]
fn codex_strings() {
    let codex = builtin("codex");
    let cases = [
        (
            "■ stream disconnected before completion: error sending request",
            Trigger::Network,
        ),
        (
            "■ stream disconnected before completion: Transport error: network error: error decoding response body",
            Trigger::Network,
        ),
        (
            "■ exceeded retry limit, last status: 429 Too Many Requests",
            Trigger::RateLimited,
        ),
        (
            "■ exceeded retry limit, last status: 500 Internal Server Error",
            Trigger::ServerError,
        ),
        (
            "■ exceeded retry limit, last status: 503 Service Unavailable",
            Trigger::ServerError,
        ),
        (
            "■ exceeded retry limit, last status: 529",
            Trigger::Overloaded,
        ),
        (
            "■ We’re currently experiencing high demand, which may cause temporary errors.",
            Trigger::ServerError,
        ),
        (
            "■ Selected model is at capacity. Please try a different model.",
            Trigger::Overloaded,
        ),
        (
            "■ You’ve hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://chatgpt.com/settings/usage to purchase more credits or try again at 11:49 PM.",
            Trigger::UsageLimit,
        ),
    ];
    for (line, expected) in cases {
        assert_eq!(trigger(&codex, line), Some(expected), "{line}");
    }
    assert_eq!(
        run(&codex, "• Working (12s • esc to interrupt)"),
        Classification::Working
    );
    assert_eq!(
        run(&codex, "• Reconnecting... 2/5"),
        Classification::Working
    );
    let limit = problem(
        &codex,
        "■ You’ve hit your usage limit. Try again at 3:05 PM.",
    );
    assert_eq!(
        limit.resets_at_ms,
        Some(noon() + 3 * 3_600_000 + 5 * 60_000)
    );
}

#[test]
fn codex_never_continues_on_errors_that_need_a_human() {
    let codex = builtin("codex");
    for line in [
        "■ Quota exceeded. Check your plan and billing details.",
        "■ Your workspace is out of credits. Add credits to continue.",
        "■ You hit your spend cap for this month.",
        "■ You’ve hit your usage limit for gpt-5.1-codex. Switch to another model now, or try again later.",
        "■ Conversation interrupted - use /feedback if something went wrong",
        "  └ Stream disconnected before completion: Transport error",
    ] {
        assert_eq!(run(&codex, line), Classification::Idle, "{line}");
    }
}
