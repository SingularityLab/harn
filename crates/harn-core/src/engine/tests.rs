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

fn policy() -> Policy {
    let mut policy = Policy {
        settle_ms: SETTLE,
        effect_timeout_ms: TIMEOUT,
        recovery_ms: 60_000,
        stall_after_ms: 120_000,
        user_grace_ms: 10_000,
        max_actions_per_hour: 40,
        reactions: Reactions::default(),
    };
    policy.reactions.overloaded = Reaction {
        action: ReactionKind::Continue,
        text: None,
        delay_secs: 10,
        max_delay_secs: 30,
        max_attempts: 4,
        fallback_delay_secs: None,
    };
    policy
}

/// Agent output rows followed by an empty input box (3 rows).
fn screen(rows: &[&str]) -> String {
    format!("{}\n─────\n>\n─────", rows.join("\n"))
}
fn err() -> String {
    screen(&["> go", "! overloaded"])
}
fn working() -> String {
    screen(&["> go", "* thinking (esc to interrupt)"])
}
fn idle() -> String {
    screen(&["> go", "Done."])
}
/// The old error stays on screen above the echoed `continue` and the reply.
fn resumed() -> String {
    screen(&["> go", "! overloaded", "> continue", "Done."])
}
/// `continue` failed with the very same error text.
fn again() -> String {
    screen(&["> go", "! overloaded", "> continue", "! overloaded"])
}

fn input(attempt: u32) -> Action {
    Action::Input {
        chunks: vec!["continue".into(), "\r".into()],
        gap_ms: 150,
        trigger: Trigger::Overloaded,
        attempt,
    }
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

    /// Time passes without output, then the adapter polls.
    fn wait(&mut self, ms: u64) -> Vec<Action> {
        self.now += ms;
        self.step()
    }

    /// Sleeps until the engine's next deadline, then polls.
    fn wake(&mut self) -> Vec<Action> {
        let at = self
            .engine
            .next_deadline_ms()
            .expect("engine should have a deadline");
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
        const MAX_WAKES: usize = 1_000;
        let end = self.now + ms;
        let mut actions = Vec::new();
        for _ in 0..MAX_WAKES {
            match self.deadline().filter(|&at| at <= end) {
                Some(at) => {
                    assert!(
                        at >= self.now,
                        "deadline {at} is in the past (now {})",
                        self.now
                    );
                    self.now = at;
                    actions.extend(self.step());
                }
                None => {
                    self.now = end;
                    actions.extend(self.step());
                    return actions;
                }
            }
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

fn is_notify(actions: &[Action], trigger: Trigger) -> bool {
    matches!(actions, [Action::Notify { trigger: t, .. }] if *t == trigger)
}

#[test]
fn acts_only_on_a_settled_screen_after_the_delay() {
    let mut sim = Sim::new();
    assert_eq!(sim.deadline(), None);
    assert_eq!(sim.print(&err()), []);
    assert_eq!(sim.deadline(), Some(T0 + SETTLE));
    assert_eq!(sim.wake(), []);
    assert_eq!(sim.deadline(), Some(T0 + SETTLE + 10_000));
    assert_eq!(sim.wake(), [input(1)]);
}

#[test]
fn a_screen_that_keeps_changing_is_never_acted_on() {
    let mut sim = Sim::new();
    for _ in 0..60 {
        assert_eq!(sim.print(&err()), []);
        sim.now += SETTLE / 2;
    }
}

#[test]
fn failed_attempts_back_off_exponentially_up_to_the_cap_then_notify_once() {
    let mut sim = Sim::new();
    assert_eq!(sim.first_action(), [input(1)]);
    // The screen never changes: each attempt fails after the effect timeout.
    let mut acted_at = sim.now;
    for (attempt, delay) in [(2, 20_000), (3, 30_000), (4, 30_000)] {
        assert_eq!(
            sim.wake(),
            [],
            "attempt {attempt} must wait for its backoff"
        );
        assert_eq!(sim.now, acted_at + TIMEOUT);
        assert_eq!(sim.deadline(), Some(sim.now + delay));
        assert_eq!(sim.wake(), [input(attempt)]);
        acted_at = sim.now;
    }
    assert!(is_notify(&sim.wake(), Trigger::Overloaded));
    assert_eq!(sim.deadline(), None);
    assert_eq!(sim.quiet_for(HOUR), []);
}

#[test]
fn after_giving_up_clean_idle_starts_over() {
    let mut sim = Sim::new();
    sim.first_action();
    for _ in 0..7 {
        sim.wake();
    }
    assert_eq!(sim.deadline(), None, "should have given up");
    // Working again after giving up: the next occurrence only notifies.
    sim.print(&working());
    sim.print(&again());
    assert!(is_notify(&sim.wake(), Trigger::Overloaded));
    // A clean idle screen resets the counter.
    sim.print(&idle());
    sim.wake();
    assert_eq!(sim.first_action(), [input(1)]);
}

#[test]
fn self_recovery_before_due_cancels() {
    let mut sim = Sim::new();
    sim.print(&err());
    sim.wake();
    assert_eq!(sim.print(&working()), []);
    assert_eq!(sim.print(&resumed()), []);
    assert_eq!(sim.quiet_for(HOUR), []);

    let mut sim = Sim::new();
    sim.print(&err());
    sim.wake();
    sim.print(&idle());
    assert_eq!(sim.wake(), []);
    assert_eq!(sim.deadline(), None);
    assert_eq!(sim.quiet_for(HOUR), []);
}

#[test]
fn rearms_after_working_and_acts_on_the_next_occurrence() {
    let mut sim = Sim::new();
    sim.first_action();
    sim.now += 2_000;
    assert_eq!(sim.print(&working()), []);
    sim.now += 5_000;
    assert_eq!(sim.print(&again()), []);
    assert_eq!(sim.wake(), []);
    assert_eq!(sim.deadline(), Some(sim.now + 20_000));
    assert_eq!(sim.wake(), [input(2)]);
}

#[test]
fn the_old_error_never_counts_again_after_continue() {
    let mut sim = Sim::new();
    sim.first_action();
    sim.now += 1_000;
    sim.print(&working());
    sim.now += 3_000;
    assert_eq!(sim.print(&resumed()), []);
    assert_eq!(sim.quiet_for(HOUR), []);
}

#[test]
fn without_input_row_signatures_typing_degrades_to_one_notify() {
    let blind = PROFILE.replace("user_echo = ['^>(?:\\s|$)']\n", "");
    for saw_working in [true, false] {
        let mut sim = Sim::with(&blind, policy());
        let first = sim.first_action();
        assert!(
            matches!(&first[..], [Action::Notify { trigger: Trigger::Overloaded, message }]
                if message.contains("notify only")),
            "{first:?}"
        );
        sim.now += 1_000;
        if saw_working {
            sim.print(&working());
        }
        // The old error scrolled up under new output: history, not a new occurrence.
        assert_eq!(sim.print(&resumed()), [], "saw_working={saw_working}");
        assert_eq!(sim.quiet_for(HOUR), [], "saw_working={saw_working}");
    }
}

#[test]
fn without_input_row_signatures_a_typing_stall_reaction_notifies() {
    let blind = PROFILE.replace("empty_input = ['^>\\s*$']\n", "");
    let mut policy = policy();
    policy.reactions.stalled.action = ReactionKind::Interrupt;
    let mut sim = Sim::with(&blind, policy);
    sim.print(&working());
    sim.wake();
    assert!(is_notify(&sim.wake(), Trigger::Stalled));
}

#[test]
fn without_an_input_row_on_screen_nothing_is_typed() {
    // Exited to a shell, or a dialog replaced the prompt: the problem is seen, typing is not.
    let mut sim = Sim::new();
    sim.print("  build output\n! overloaded\nPS C:\\work> ");
    assert_eq!(sim.quiet_for(2 * HOUR), []);
    // The prompt is back (dialog closed): act at once, the delay has long passed.
    sim.print(&err());
    assert_eq!(sim.wake(), [input(1)]);
}

#[test]
fn a_different_error_right_after_acting_is_a_new_occurrence() {
    let mut sim = Sim::new();
    sim.first_action();
    sim.now += 2_000;
    sim.print(&screen(&[
        "> go",
        "! overloaded",
        "> continue",
        "! overloaded (twice)",
    ]));
    assert_eq!(sim.wake(), []);
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
        let glyph = if i % 2 == 0 { "✶" } else { "✢" };
        actions.extend(sim.print(&busy(glyph, i * 10)));
    }
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert!(is_notify(&actions, Trigger::Stalled));

    // Real progress starts a new stall window; a frozen screen still wakes the engine.
    sim.now += 1_000;
    assert_eq!(
        sim.print(&screen(&[
            "> go",
            "Edited foo.rs",
            "✻ Thinking… (esc to interrupt)"
        ])),
        []
    );
    assert_eq!(sim.wake(), []);
    assert_eq!(sim.deadline(), Some(sim.now - SETTLE + 120_000));
    assert!(is_notify(&sim.wake(), Trigger::Stalled));
    assert_eq!(sim.deadline(), None);
}

#[test]
fn the_hourly_cap_postpones_typing() {
    let mut policy = policy();
    policy.max_actions_per_hour = 2;
    let mut sim = Sim::with(PROFILE, policy);
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
    assert_eq!(sim.wait(2_000), []);
    assert_eq!(sim.deadline(), Some(T0 + 19_000));
    assert_eq!(sim.wake(), [input(1)]);
}

#[test]
fn a_draft_in_the_input_box_blocks_typing_until_it_is_gone() {
    let mut sim = Sim::new();
    sim.print("> go\n! overloaded\n─────\n> half-typed thought\n─────");
    assert_eq!(sim.wake(), []);
    assert_eq!(sim.deadline(), None);
    assert_eq!(sim.quiet_for(60_000), []);
    sim.print(&err());
    assert_eq!(sim.wake(), [input(1)]);
}

#[test]
fn interrupt_reaction_and_text_override() {
    let mut policy = policy();
    policy.reactions.overloaded.action = ReactionKind::Interrupt;
    policy.reactions.overloaded.text = Some("go on".into());
    let mut sim = Sim::with(PROFILE, policy);
    let expected = Action::Input {
        chunks: vec!["\u{1b}".into(), "go on".into(), "\r".into()],
        gap_ms: 150,
        trigger: Trigger::Overloaded,
        attempt: 1,
    };
    assert_eq!(sim.first_action(), [expected]);
}

#[test]
fn notify_reaction_fires_once_per_occurrence() {
    let mut policy = policy();
    policy.reactions.overloaded.action = ReactionKind::Notify;
    let mut sim = Sim::with(PROFILE, policy);
    let first = sim.first_action();
    assert!(
        matches!(&first[..], [Action::Notify { message, .. }] if message == "Test: overloaded: ! overloaded")
    );
    assert_eq!(sim.quiet_for(HOUR), []);
    sim.print(&working());
    sim.print(&again());
    sim.wake();
    assert!(is_notify(&sim.wake(), Trigger::Overloaded));
}

#[test]
fn ignore_reaction_does_nothing() {
    let mut policy = policy();
    policy.reactions.overloaded.action = ReactionKind::Ignore;
    let mut sim = Sim::with(PROFILE, policy);
    sim.print(&err());
    assert_eq!(sim.wake(), []);
    assert_eq!(sim.deadline(), None);
    assert_eq!(sim.quiet_for(HOUR), []);
}

#[test]
fn usage_limit_waits_for_the_reset_plus_margin_or_falls_back() {
    let usage_input = |attempt| Action::Input {
        chunks: vec!["continue".into(), "\r".into()],
        gap_ms: 150,
        trigger: Trigger::UsageLimit,
        attempt,
    };
    let mut sim = Sim::new();
    sim.print(&screen(&["> go", "! usage limit, resets in 30 min"]));
    sim.wake();
    assert_eq!(sim.deadline(), Some(T0 + SETTLE + 30 * 60_000 + 60_000));
    assert_eq!(sim.wake(), [usage_input(1)]);

    for line in ["! usage limit", "! usage limit, reset at epoch 999000"] {
        let mut sim = Sim::new();
        sim.print(&screen(&["> go", line]));
        sim.wake();
        assert_eq!(sim.deadline(), Some(T0 + SETTLE + 900_000), "{line}");
    }
}

#[test]
fn a_different_problem_while_scheduled_is_rescheduled() {
    let mut sim = Sim::new();
    sim.print(&err());
    sim.wake();
    sim.now = T0 + 8_000;
    sim.print(&screen(&["> go", "! server error"]));
    sim.wake();
    assert_eq!(sim.deadline(), Some(T0 + 9_000 + 20_000));
}

#[test]
fn stopped_early_can_be_continued_when_enabled() {
    let mut policy = policy();
    policy.reactions.stopped_early.action = ReactionKind::Continue;
    let mut sim = Sim::with(PROFILE, policy);
    sim.print(&screen(&["> go", "Part one done. want me to continue?"]));
    sim.wake();
    let acted = sim.wake();
    assert!(matches!(
        &acted[..],
        [Action::Input {
            trigger: Trigger::StoppedEarly,
            attempt: 1,
            ..
        }]
    ));
}

#[test]
fn backoff_saturates_instead_of_overflowing() {
    let mut reaction = policy().reactions.overloaded;
    reaction.delay_secs = u64::MAX / 2;
    reaction.max_delay_secs = u64::MAX;
    let problem = Problem {
        trigger: Trigger::Overloaded,
        line: "! overloaded".into(),
        lines_below: 0,
        resets_at_ms: None,
    };
    assert_eq!(due_ms(&reaction, &problem, u32::MAX, T0), u64::MAX);
    assert_eq!(due_ms(&reaction, &problem, 1, u64::MAX - 1), u64::MAX);
}

#[test]
fn a_zero_hourly_cap_never_types() {
    let mut policy = policy();
    policy.max_actions_per_hour = 0;
    let mut sim = Sim::with(PROFILE, policy);
    sim.print(&err());
    assert_eq!(sim.wake(), []);
    assert_eq!(sim.deadline(), None);
    assert_eq!(sim.quiet_for(HOUR), []);
}

/// Our `continue` still sits unsent in the input box: the Enter was swallowed.
fn swallowed() -> String {
    "> go\n! overloaded\n─────\n> continue\n─────".to_owned()
}

fn submit_only(attempt: u32) -> Action {
    Action::Input {
        chunks: vec!["\r".into()],
        gap_ms: 150,
        trigger: Trigger::Overloaded,
        attempt,
    }
}

#[test]
fn a_swallowed_enter_is_resent_alone_then_the_normal_flow_resumes() {
    let mut sim = Sim::new();
    assert_eq!(sim.first_action(), [input(1)]);
    sim.now += 200;
    assert_eq!(sim.print(&swallowed()), []);
    assert_eq!(sim.wake(), [submit_only(1)]);
    sim.now += 300;
    assert_eq!(sim.print(&working()), []);
    sim.now += 5_000;
    sim.print(&again());
    assert_eq!(sim.wake(), []);
    assert_eq!(sim.wake(), [input(2)]);
}

#[test]
fn a_user_draft_after_our_continue_is_never_typed_into() {
    let mut sim = Sim::new();
    assert_eq!(sim.first_action(), [input(1)]);
    sim.now += 2_000;
    sim.print("> go\n! overloaded\n─────\n> fix the tests\n─────");
    assert_eq!(sim.quiet_for(HOUR), []);
}

#[test]
fn enter_is_resent_at_most_twice_and_counts_toward_the_hourly_cap() {
    let mut sim = Sim::new();
    sim.first_action();
    sim.now += 200;
    sim.print(&swallowed());
    let actions = sim.quiet_for(HOUR);
    assert_eq!(actions[..2], [submit_only(1), submit_only(1)]);
    // Then one notification instead of waiting silently, and nothing more.
    assert!(
        matches!(&actions[2..], [Action::Notify { trigger: Trigger::Overloaded, message }]
            if message.contains("typed `continue` but the agent did not submit it")),
        "{actions:?}"
    );

    // Cap 2: the action and the first re-send use it up; the second re-send would be due long
    // before the hour frees a slot.
    let mut policy = policy();
    policy.max_actions_per_hour = 2;
    let mut sim = Sim::with(PROFILE, policy);
    sim.first_action();
    sim.now += 200;
    sim.print(&swallowed());
    assert_eq!(sim.quiet_for(HOUR / 2), [submit_only(1)]);
}
