use harn_core::Trigger;

use super::*;

const POLL: Duration = Duration::from_secs(2);
const CLOCK: &str = "14:32";

fn event(state: &mut TrayState, event: Event, now: Instant) {
    state.apply(Report::Event(event), now, CLOCK);
}

fn attached(handle: &str, agent: &str) -> Event {
    Event::Attached {
        handle: handle.into(),
        agent: agent.into(),
    }
}

fn tick(agents: &[&str]) -> Event {
    Event::Tick {
        active_agents: agents.iter().map(|a| (*a).to_owned()).collect(),
    }
}

fn acted(handle: &str, action: Action) -> Event {
    Event::Acted {
        handle: handle.into(),
        agent: "claude".into(),
        action,
        dry_run: false,
    }
}

fn input() -> Action {
    Action::Input {
        chunks: vec!["continue".into(), "\r".into()],
        gap_ms: 0,
        trigger: Trigger::Overloaded,
        attempt: 1,
    }
}

fn gave_up() -> Action {
    Action::Notify {
        trigger: Trigger::UsageLimit,
        message: "gave up".into(),
    }
}

#[test]
fn connects_then_watches_sessions() {
    let now = Instant::now();
    let mut state = TrayState::new(POLL);
    assert_eq!(state.status(now), Status::Connecting);
    event(&mut state, attached("term_a", "claude"), now);
    event(&mut state, attached("term_b", "codex"), now);
    event(&mut state, tick(&["claude", "codex"]), now);
    assert_eq!(state.status(now), Status::Watching { sessions: 2 });
    assert_eq!(state.status(now).text(), "Watching 2 agent sessions");
    assert_eq!(state.status(now).colour(), Colour::Green);
    assert_eq!(state.active_agents(), ["claude", "codex"]);
    event(
        &mut state,
        Event::Detached {
            handle: "term_b".into(),
        },
        now,
    );
    assert_eq!(state.status(now).text(), "Watching 1 agent session");
}

#[test]
fn unreachable_or_silent_orca_is_amber() {
    let now = Instant::now();
    let mut state = TrayState::new(POLL);
    state.apply(Report::Unreachable, now, CLOCK);
    assert_eq!(state.status(now).text(), "Orca not running — retrying");
    assert_eq!(state.status(now).colour(), Colour::Amber);
    event(&mut state, tick(&[]), now);
    assert_eq!(state.status(now).colour(), Colour::Green);
    let later = now + MIN_STALE_AFTER + Duration::from_secs(1);
    assert_eq!(state.status(later), Status::OrcaDown);
}

#[test]
fn slow_polling_is_not_mistaken_for_silence() {
    let now = Instant::now();
    let mut state = TrayState::new(Duration::from_secs(60));
    event(&mut state, tick(&[]), now);
    let next_poll_due = now + Duration::from_secs(61);
    assert_eq!(state.status(next_poll_due).colour(), Colour::Green);
}

#[test]
fn needing_the_user_is_red_until_acknowledged() {
    let now = Instant::now();
    let mut state = TrayState::new(POLL);
    event(&mut state, tick(&["claude"]), now);
    event(&mut state, acted("term_a", gave_up()), now);
    assert_eq!(
        state.status(now),
        Status::NeedsYou {
            agent: "claude".into()
        }
    );
    assert_eq!(state.status(now).colour(), Colour::Red);
    assert_eq!(
        state.last_line(),
        "Last: claude needs you (usage limit) at 14:32"
    );
    event(&mut state, tick(&["claude"]), now);
    assert_eq!(state.status(now).colour(), Colour::Red);
    state.acknowledge();
    assert_eq!(state.status(now).colour(), Colour::Green);
}

#[test]
fn red_clears_when_that_terminal_recovers_or_closes() {
    let now = Instant::now();
    let mut state = TrayState::new(POLL);
    event(&mut state, tick(&[]), now);
    event(&mut state, acted("term_a", gave_up()), now);
    event(&mut state, acted("term_b", input()), now);
    assert_eq!(state.status(now).colour(), Colour::Red);
    event(&mut state, acted("term_a", input()), now);
    assert_eq!(state.status(now).colour(), Colour::Green);
    assert_eq!(state.last_line(), "Last: continued claude at 14:32");
    event(&mut state, acted("term_a", gave_up()), now);
    event(
        &mut state,
        Event::Detached {
            handle: "term_a".into(),
        },
        now,
    );
    assert_eq!(state.status(now).colour(), Colour::Green);
}

#[test]
fn dry_runs_are_not_reported_as_actions() {
    let now = Instant::now();
    let mut state = TrayState::new(POLL);
    let dry = Event::Acted {
        handle: "term_a".into(),
        agent: "claude".into(),
        action: gave_up(),
        dry_run: true,
    };
    event(&mut state, dry, now);
    assert_eq!(state.last_line(), "Last: nothing yet");
    assert_eq!(state.status(now), Status::Connecting);
}

#[test]
fn pause_and_stop_win_over_everything() {
    let now = Instant::now();
    let mut state = TrayState::new(POLL);
    event(&mut state, acted("term_a", gave_up()), now);
    state.paused();
    assert_eq!(state.status(now), Status::Paused);
    assert_eq!(state.status(now).colour(), Colour::Grey);
    state.started(POLL);
    assert_eq!(state.status(now), Status::Connecting);
    state.apply(
        Report::Stopped("no profile for agent \"x\"".into()),
        now,
        CLOCK,
    );
    assert_eq!(state.status(now).colour(), Colour::Red);
    assert!(state.status(now).text().starts_with("Stopped: no profile"));
}

#[test]
fn only_real_updates_become_the_last_action() {
    let mut state = TrayState::new(POLL);
    let same = Outcome::UpToDate {
        version: "1.0.0".into(),
    };
    state.record_update("codex", &same, CLOCK);
    assert_eq!(state.last_line(), "Last: nothing yet");
    let updated = Outcome::Updated {
        from: "1.0.0".into(),
        to: "1.1.0".into(),
    };
    state.record_update("codex", &updated, CLOCK);
    assert_eq!(state.last_line(), "Last: updated codex to 1.1.0 at 14:32");
}
