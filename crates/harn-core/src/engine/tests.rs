use super::*;
use crate::policy::Reactions;

const PROFILE: &str = r#"
id = "test"
name = "Test"
[signatures]
working = ['esc to interrupt']
overloaded = ['^! overloaded']
server_error = ['^! server error']
usage_limit = ['^! usage limit']
stopped_early = ['want me to continue\?$']
user_echo = ['^>(?:\s|$)']
empty_input = ['^>\s*$']
chrome = ['^─+$']
reset_hints = ['resets in (?P<minutes>\d+) min', 'reset at epoch (?P<epoch>\d+)']
"#;

const T0: u64 = 1_000_000_000;
const SETTLE: u64 = 1_000;
const TIMEOUT: u64 = 30_000;
const HOUR: u64 = 3_600_000;

/// Overloaded: continue after 10 s, doubling up to 30 s, at most 4 attempts.
fn policy() -> Policy {
    let overloaded = Reaction {
        action: ReactionKind::Continue,
        text: None,
        delay_secs: 10,
        max_delay_secs: 30,
        max_attempts: 4,
        fallback_delay_secs: None,
    };
    Policy {
        settle_ms: SETTLE,
        effect_timeout_ms: TIMEOUT,
        recovery_ms: 60_000,
        stall_after_ms: 120_000,
        user_grace_ms: 10_000,
        max_actions_per_hour: 40,
        reactions: Reactions {
            overloaded,
            ..Reactions::default()
        },
    }
}

/// Agent output rows followed by an empty input box.
fn screen(rows: &[&str]) -> String {
    format!("{}\n─────\n>\n─────", rows.join("\n"))
}
fn err() -> String {
    screen(&["> go", "! overloaded"])
}
fn working() -> String {
    screen(&["> go", "* thinking (esc to interrupt)"])
}
/// The old error stays on screen above the echoed `continue` and the reply.
fn resumed() -> String {
    screen(&["> go", "! overloaded", "> continue", "Done."])
}
/// `continue` failed with the very same error text.
fn again() -> String {
    screen(&["> go", "! overloaded", "> continue", "! overloaded"])
}
/// Our `continue` still sits unsent in the input box: the Enter was swallowed.
fn swallowed() -> String {
    "> go\n! overloaded\n─────\n> continue\n─────".into()
}

fn typed(chunks: &[&str], trigger: Trigger, attempt: u32) -> Action {
    Action::Input {
        chunks: chunks.iter().map(|c| (*c).to_owned()).collect(),
        gap_ms: 150,
        trigger,
        attempt,
    }
}
fn input(attempt: u32) -> Action {
    typed(&["continue", "\r"], Trigger::Overloaded, attempt)
}
fn submit_only(attempt: u32) -> Action {
    typed(&["\r"], Trigger::Overloaded, attempt)
}
fn notified(actions: &[Action], trigger: Trigger, text: &str) -> bool {
    matches!(actions, [Action::Notify { trigger: t, message }] if *t == trigger && message.contains(text))
}

struct Sim {
    engine: Engine,
    now: u64,
    screen: String,
    last_output: u64,
}

impl Sim {
    fn new() -> Self {
        Self::with(PROFILE, policy())
    }

    fn with(profile: &str, policy: Policy) -> Self {
        let profile = Profile::from_toml(profile).unwrap();
        let tz = TimeZone::get("Europe/Moscow").unwrap();
        Self {
            engine: Engine::with_time_zone(profile, policy, tz),
            now: T0,
            screen: String::new(),
            last_output: 0,
        }
    }

    /// The agent redraws its screen now.
    fn print(&mut self, screen: &str) -> Vec<Action> {
        self.screen = screen.to_owned();
        self.last_output = self.now;
        self.step()
    }

    /// Sleeps until the engine's next deadline, then polls.
    fn wake(&mut self) -> Vec<Action> {
        let at = self.deadline().expect("engine should have a deadline");
        assert!(
            at >= self.now,
            "deadline {at} is in the past (now {})",
            self.now
        );
        self.now = at;
        self.step()
    }

    fn step(&mut self) -> Vec<Action> {
        let snap = Snapshot {
            text: self.screen.clone(),
            last_output_ms: self.last_output,
        };
        self.engine.step(&snap, self.now)
    }

    fn deadline(&self) -> Option<u64> {
        self.engine.next_deadline_ms()
    }

    /// The screen stays as it is for `ms`: wakes at every deadline in that window (as an adapter
    /// would), then polls once at the end. Returns everything the engine did.
    fn quiet_for(&mut self, ms: u64) -> Vec<Action> {
        let end = self.now + ms;
        let mut actions = Vec::new();
        for _ in 0..1_000 {
            let Some(at) = self.deadline().filter(|&at| at <= end) else {
                self.now = end;
                actions.extend(self.step());
                return actions;
            };
            assert!(
                at >= self.now,
                "deadline {at} is in the past (now {})",
                self.now
            );
            self.now = at;
            actions.extend(self.step());
        }
        panic!("engine kept asking to be woken: busy loop");
    }

    /// Shows the error and runs the clock until the first action (T0 + 11 s).
    fn first_action(&mut self) -> Vec<Action> {
        self.print(&err());
        self.wake();
        self.wake()
    }
}

#[test]
fn acts_only_on_a_settled_screen_after_the_delay() {
    let mut sim = Sim::new();
    for _ in 0..60 {
        assert_eq!(
            sim.print(&err()),
            [],
            "a screen that keeps changing is never acted on"
        );
        sim.now += SETTLE / 2;
    }
    let mut sim = Sim::new();
    assert_eq!(sim.print(&err()), []);
    assert_eq!(sim.deadline(), Some(T0 + SETTLE));
    assert_eq!(sim.wake(), []);
    assert_eq!(sim.deadline(), Some(T0 + SETTLE + 10_000));
    assert_eq!(sim.wake(), [input(1)]);
}

#[test]
fn failed_attempts_back_off_then_notify_once_until_clean_idle() {
    let mut sim = Sim::new();
    assert_eq!(sim.first_action(), [input(1)]);
    // The screen never changes: each attempt fails after the effect timeout.
    for (attempt, delay) in [(2, 20_000), (3, 30_000), (4, 30_000)] {
        let acted_at = sim.now;
        assert_eq!(
            sim.wake(),
            [],
            "attempt {attempt} must wait for its backoff"
        );
        assert_eq!(sim.now, acted_at + TIMEOUT);
        assert_eq!(sim.deadline(), Some(sim.now + delay));
        assert_eq!(sim.wake(), [input(attempt)]);
    }
    assert!(notified(
        &sim.wake(),
        Trigger::Overloaded,
        "still failing after 4 attempts"
    ));
    assert_eq!(sim.quiet_for(HOUR), []);
    // Working again, then the same error: still over the limit, so only a notification.
    sim.print(&working());
    sim.print(&again());
    assert!(notified(&sim.wake(), Trigger::Overloaded, "still failing"));
    // A clean idle screen resets the counter.
    sim.print(&screen(&["> go", "Done."]));
    sim.wake();
    assert_eq!(sim.first_action(), [input(1)]);
}

#[test]
fn self_recovery_before_due_cancels() {
    for recovery in [working(), screen(&["> go", "Done."])] {
        let mut sim = Sim::new();
        sim.print(&err());
        sim.wake();
        sim.print(&recovery);
        sim.print(&resumed());
        assert_eq!(sim.quiet_for(HOUR), [], "{recovery}");
    }
}

#[test]
fn occurrences_after_acting() {
    // The agent resumed and failed again in the same place: a new occurrence, larger backoff.
    let mut sim = Sim::new();
    sim.first_action();
    sim.print(&working());
    sim.now += 5_000;
    sim.print(&again());
    assert_eq!(sim.wake(), []);
    assert_eq!(sim.deadline(), Some(sim.now + 20_000));
    assert_eq!(sim.wake(), [input(2)]);
    // The old error above the echoed `continue` never counts again.
    let mut sim = Sim::new();
    sim.first_action();
    sim.print(&working());
    sim.print(&resumed());
    assert_eq!(sim.quiet_for(HOUR), []);
    // A different error right after acting is new even without Working seen in between.
    let mut sim = Sim::new();
    sim.first_action();
    sim.print(&screen(&[
        "> go",
        "! overloaded",
        "> continue",
        "! overloaded (twice)",
    ]));
    sim.wake();
    assert_eq!(sim.wake(), [input(2)]);
}

#[test]
fn attempts_reset_after_recovery_ms_of_continuous_work() {
    for (work_secs, expected_attempt) in [(70, 1), (30, 2)] {
        let mut sim = Sim::new();
        sim.first_action();
        for _ in 0..=work_secs / 10 {
            sim.now += 10_000;
            sim.print(&working());
        }
        sim.now += 1_000;
        sim.print(&again());
        sim.wake();
        assert_eq!(
            sim.wake(),
            [input(expected_attempt)],
            "worked {work_secs} s"
        );
    }
}

#[test]
fn a_frozen_working_screen_is_stalled_once_per_stall() {
    let mut sim = Sim::new();
    let busy = |glyph: &str, secs: u64| {
        screen(&[
            "> go",
            &format!("{glyph} Thinking… {secs}s (esc to interrupt)"),
        ])
    };
    let mut actions = sim.print(&busy("✻", 1));
    for i in 1..=20 {
        sim.now += 10_000;
        actions.extend(sim.print(&busy(if i % 2 == 0 { "✶" } else { "✢" }, i * 10)));
    }
    assert!(
        notified(&actions, Trigger::Stalled, "screen unchanged"),
        "{actions:?}"
    );
    assert_eq!(sim.quiet_for(HOUR), [], "once per stall");
}

#[test]
fn the_hourly_cap_postpones_typing() {
    let mut sim = Sim::with(
        PROFILE,
        Policy {
            max_actions_per_hour: 2,
            ..policy()
        },
    );
    assert_eq!(sim.first_action(), [input(1)]);
    let first_at = sim.now;
    sim.wake();
    assert_eq!(sim.wake(), [input(2)]);
    sim.wake();
    assert_eq!(sim.deadline(), Some(first_at + HOUR));
    assert_eq!(sim.wake(), [input(3)]);
}

#[test]
fn user_input_postpones_typing() {
    let mut sim = Sim::new();
    sim.print(&err());
    sim.wake();
    sim.now = T0 + 9_000;
    sim.engine.note_user_input(sim.now);
    sim.now += 2_000;
    assert_eq!(sim.step(), []);
    assert_eq!(sim.deadline(), Some(T0 + 19_000));
    assert_eq!(sim.wake(), [input(1)]);
}

#[test]
fn never_types_into_a_draft_or_a_missing_input_box() {
    let draft = "> go\n! overloaded\n─────\n> half-typed thought\n─────";
    // Exited to a shell, or a dialog replaced the prompt: the problem is seen, typing is not.
    let gone = "  build output\n! overloaded\nPS C:\\work> ";
    for blocked in [draft, gone] {
        let mut sim = Sim::new();
        sim.print(blocked);
        assert_eq!(sim.quiet_for(2 * HOUR), [], "{blocked}");
        // The prompt is empty again: act at once, the delay has long passed.
        sim.print(&err());
        assert_eq!(sim.wake(), [input(1)], "{blocked}");
    }
    // The user replaced our unsent `continue` with their own words.
    let mut sim = Sim::new();
    sim.first_action();
    sim.print("> go\n! overloaded\n─────\n> fix the tests\n─────");
    assert_eq!(sim.quiet_for(HOUR), []);
}

#[test]
fn without_input_row_signatures_typing_degrades_to_one_notify() {
    let blind = PROFILE.replace("user_echo = ['^>(?:\\s|$)']\n", "");
    for saw_working in [true, false] {
        let mut sim = Sim::with(&blind, policy());
        let first = sim.first_action();
        assert!(
            notified(&first, Trigger::Overloaded, "notify only"),
            "{first:?}"
        );
        if saw_working {
            sim.print(&working());
        }
        // The old error scrolled up under new output: history, not a new occurrence.
        sim.print(&resumed());
        assert_eq!(sim.quiet_for(HOUR), [], "saw_working={saw_working}");
    }
    let mut policy = policy();
    policy.reactions.stalled.action = ReactionKind::Interrupt;
    let mut sim = Sim::with(&blind, policy);
    sim.print(&working());
    sim.wake();
    assert!(notified(&sim.wake(), Trigger::Stalled, "screen unchanged"));
}

#[test]
fn a_swallowed_enter_is_resent_alone_then_the_normal_flow_resumes() {
    let mut sim = Sim::new();
    assert_eq!(sim.first_action(), [input(1)]);
    sim.now += 200;
    sim.print(&swallowed());
    assert_eq!(sim.wake(), [submit_only(1)]);
    sim.print(&working());
    sim.now += 5_000;
    sim.print(&again());
    sim.wake();
    assert_eq!(sim.wake(), [input(2)]);
}

#[test]
fn enter_is_resent_at_most_twice_then_notified_and_counts_toward_the_hourly_cap() {
    let mut sim = Sim::new();
    sim.first_action();
    sim.now += 200;
    sim.print(&swallowed());
    let actions = sim.quiet_for(HOUR);
    assert_eq!(actions[..2], [submit_only(1), submit_only(1)]);
    let gave_up = "typed `continue` but the agent did not submit it";
    assert!(
        notified(&actions[2..], Trigger::Overloaded, gave_up),
        "{actions:?}"
    );
    // Cap 2: the action and the first re-send use it up.
    let mut sim = Sim::with(
        PROFILE,
        Policy {
            max_actions_per_hour: 2,
            ..policy()
        },
    );
    sim.first_action();
    sim.now += 200;
    sim.print(&swallowed());
    assert_eq!(sim.quiet_for(HOUR / 2), [submit_only(1)]);
}

#[test]
fn reaction_kinds() {
    let with = |set: fn(&mut Reaction)| {
        let mut policy = policy();
        set(&mut policy.reactions.overloaded);
        Sim::with(PROFILE, policy)
    };
    let mut sim = with(|r| {
        r.action = ReactionKind::Interrupt;
        r.text = Some("go on".into());
    });
    let interrupt = typed(&["\u{1b}", "go on", "\r"], Trigger::Overloaded, 1);
    assert_eq!(sim.first_action(), [interrupt]);
    // Notify: once per occurrence.
    let mut sim = with(|r| r.action = ReactionKind::Notify);
    assert!(notified(
        &sim.first_action(),
        Trigger::Overloaded,
        "Test: overloaded: ! overloaded"
    ));
    assert_eq!(sim.quiet_for(HOUR), []);
    sim.print(&working());
    sim.print(&again());
    sim.wake();
    assert!(notified(&sim.wake(), Trigger::Overloaded, "! overloaded"));
}

#[test]
fn usage_limit_waits_for_the_reset_plus_margin_or_falls_back() {
    let mut sim = Sim::new();
    sim.print(&screen(&["> go", "! usage limit, resets in 30 min"]));
    sim.wake();
    assert_eq!(sim.deadline(), Some(T0 + SETTLE + 30 * 60_000 + 60_000));
    assert_eq!(
        sim.wake(),
        [typed(&["continue", "\r"], Trigger::UsageLimit, 1)]
    );
    // Unparsed, or a reset already in the past: the 900 s fallback backoff.
    for line in ["! usage limit", "! usage limit, reset at epoch 999000"] {
        let mut sim = Sim::new();
        sim.print(&screen(&["> go", line]));
        sim.wake();
        assert_eq!(sim.deadline(), Some(T0 + SETTLE + 900_000), "{line}");
    }
}
