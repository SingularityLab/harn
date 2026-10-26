use std::collections::HashMap;

use harn_core::Trigger;

use super::*;

const CLAUDE: &str = "term_claude";
const T0: u64 = 1_793_000_000_000;
const GAP_MS: u64 = 150;
/// The fake engine first sees the error at `SEEN` and acts at `DUE`.
const SEEN: u64 = T0 + 5_000;
const DUE: u64 = SEEN + 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Read(String),
    Send(Keys),
    Pause,
}

/// Orca whose agents echo typed text into their last bare `❯` row, like Claude Code.
#[derive(Default)]
struct FakeCli {
    terminals: Vec<TerminalInfo>,
    screens: HashMap<String, String>,
    /// What each agent's input box holds.
    typed: HashMap<String, String>,
    draft: bool,
    fail_reads: bool,
    /// The user types this right after our text.
    interject: Option<String>,
    calls: Vec<Call>,
}

impl OrcaCli for FakeCli {
    fn list(&mut self) -> Result<Vec<TerminalInfo>, CliError> {
        Ok(self.terminals.clone())
    }

    fn read_screen(&mut self, handle: &str) -> Result<Screen, CliError> {
        self.calls.push(Call::Read(handle.to_owned()));
        let screen = self.screens.get(handle).filter(|_| !self.fail_reads);
        let screen = screen.ok_or(CliError::NotScreen(None))?;
        let typed = self.typed.get(handle).map_or("", String::as_str);
        let text = match screen.rfind("\n❯\n") {
            Some(at) if !typed.is_empty() => {
                format!(
                    "{}\n❯ {typed}\n{}",
                    &screen[..at],
                    &screen[at + "\n❯\n".len()..]
                )
            }
            _ => screen.clone(),
        };
        Ok(Screen {
            text,
            has_draft: self.draft,
        })
    }

    fn send(&mut self, handle: &str, keys: &Keys) -> Result<(), CliError> {
        self.calls.push(Call::Send(keys.clone()));
        let typed = self.typed.entry(handle.to_owned()).or_default();
        match keys {
            Keys::Text(text) if !text.chars().any(char::is_control) => {
                typed.push_str(text);
                typed.push_str(&self.interject.take().unwrap_or_default());
            }
            _ => typed.clear(),
        }
        Ok(())
    }

    fn pause(&mut self, _ms: u64) {
        self.calls.push(Call::Pause);
    }
}

/// Knows `claude` (alias `claude-code`) and `codex`; the input box is the last `❯` row.
struct FakeAgents;

impl Agents for FakeAgents {
    type Engine = FakeEngine;

    fn profile_id(&self, agent: &str) -> Option<String> {
        let id = match agent {
            "claude" | "claude-code" => "claude",
            "codex" => "codex",
            _ => return None,
        };
        Some(id.to_owned())
    }

    fn engine(&self, _: &str) -> Option<FakeEngine> {
        Some(FakeEngine::default())
    }

    fn input_row(&self, _: &str, screen: &str) -> InputRow {
        match screen.lines().rev().find_map(|row| row.strip_prefix('❯')) {
            None => InputRow::Missing,
            Some(rest) if rest.trim().is_empty() => InputRow::Empty,
            Some(rest) => InputRow::Holds(rest.trim().to_owned()),
        }
    }
}

/// Continues 1 s after first seeing "API Error".
#[derive(Default)]
struct FakeEngine {
    due: Option<u64>,
    steps: usize,
    user_inputs: Vec<u64>,
}

impl AgentEngine for FakeEngine {
    fn step(&mut self, snap: &Snapshot, now_ms: u64) -> Vec<Action> {
        self.steps += 1;
        if !snap.text.contains("API Error") {
            self.due = None;
            return Vec::new();
        }
        match self.due {
            Some(due) if now_ms >= due => {
                self.due = None;
                vec![input(&["continue", "\r"])]
            }
            Some(_) => Vec::new(),
            None => {
                self.due = Some(now_ms + DUE - SEEN);
                Vec::new()
            }
        }
    }

    fn next_deadline_ms(&self) -> Option<u64> {
        self.due
    }

    fn note_user_input(&mut self, now_ms: u64) {
        self.user_inputs.push(now_ms);
    }
}

fn input(chunks: &[&str]) -> Action {
    Action::Input {
        chunks: chunks.iter().map(|c| (*c).to_owned()).collect(),
        gap_ms: GAP_MS,
        trigger: Trigger::Overloaded,
        attempt: 1,
    }
}

fn terminal(handle: &str, agent: Option<&str>) -> TerminalInfo {
    TerminalInfo {
        handle: handle.to_owned(),
        connected: true,
        writable: true,
        last_output_at: T0,
        agent_identity: agent.map(str::to_owned),
    }
}

fn capture(json: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(json).unwrap();
    let rows = value["result"]["terminal"]["tail"].as_array().unwrap();
    let rows: Vec<&str> = rows.iter().filter_map(|r| r.as_str()).collect();
    rows.join("\n")
}

fn agent_screen() -> String {
    capture(include_str!("../../tests/data/read_screen_agent.json"))
}

fn exited_screen() -> String {
    capture(include_str!("../../tests/data/read_screen_exited.json"))
}

fn watcher(
    terminals: Vec<TerminalInfo>,
    screen: &str,
    pinned: Vec<TerminalPin>,
) -> Watcher<FakeCli, FakeAgents> {
    let screens = terminals
        .iter()
        .map(|t| (t.handle.clone(), screen.to_owned()))
        .collect();
    let cli = FakeCli {
        terminals,
        screens,
        ..FakeCli::default()
    };
    Watcher::new(cli, FakeAgents, false, pinned)
}

fn claude(screen: &str) -> Watcher<FakeCli, FakeAgents> {
    watcher(vec![terminal(CLAUDE, Some("claude"))], screen, Vec::new())
}

/// Sees the error, then acts on it; returns the acting tick's events with its calls left in `calls`.
fn act(w: &mut Watcher<FakeCli, FakeAgents>) -> Vec<Event> {
    w.tick(SEEN).unwrap();
    w.cli.calls.clear();
    w.tick(DUE).unwrap()
}

fn sends(w: &Watcher<FakeCli, FakeAgents>) -> Vec<&Keys> {
    w.cli
        .calls
        .iter()
        .filter_map(|c| if let Call::Send(k) = c { Some(k) } else { None })
        .collect()
}

fn reads(w: &Watcher<FakeCli, FakeAgents>) -> usize {
    w.cli
        .calls
        .iter()
        .filter(|c| matches!(c, Call::Read(_)))
        .count()
}

fn tick(agents: &[&str]) -> Event {
    Event::Tick {
        active_agents: agents.iter().map(|a| (*a).to_owned()).collect(),
    }
}

fn attached(handle: &str, agent: &str) -> Event {
    Event::Attached {
        handle: handle.to_owned(),
        agent: agent.to_owned(),
    }
}

fn detached(handle: &str) -> Event {
    Event::Detached {
        handle: handle.to_owned(),
    }
}

fn acted(action: Action, dry_run: bool) -> Event {
    let (handle, agent) = (CLAUDE.to_owned(), "claude".to_owned());
    Event::Acted {
        handle,
        agent,
        action,
        dry_run,
    }
}

#[test]
fn watches_writable_terminals_of_known_agents_or_pins() {
    let terminals = vec![
        terminal("term_shell", None),
        terminal(CLAUDE, Some("claude")),
        terminal("term_codex", Some("codex")),
        terminal("term_gemini", Some("gemini")),
        TerminalInfo {
            writable: false,
            ..terminal("term_ro", Some("claude"))
        },
        TerminalInfo {
            connected: false,
            ..terminal("term_dc", Some("claude"))
        },
    ];
    let mut w = watcher(terminals.clone(), "idle", Vec::new());
    let expected = [
        attached(CLAUDE, "claude"),
        attached("term_codex", "codex"),
        tick(&["claude", "codex"]),
    ];
    assert_eq!(w.tick(T0).unwrap(), expected);

    let pin = TerminalPin {
        handle: "term_shell".to_owned(),
        agent: "claude-code".to_owned(),
    };
    let mut w = watcher(terminals, "idle", vec![pin]);
    let events = w.tick(T0).unwrap();
    assert_eq!(events[0], attached("term_shell", "claude"));
    assert_eq!(reads(&w), 1, "only the pinned terminal is read");
}

#[test]
fn reads_only_when_output_moves_or_the_engine_is_due() {
    let mut w = claude("idle");
    w.tick(T0 + 1).unwrap();
    w.tick(T0 + 2).unwrap();
    assert_eq!(reads(&w), 1);
    w.cli.terminals[0].last_output_at = T0 + 3;
    w.tick(T0 + 4).unwrap();
    assert_eq!(reads(&w), 2);

    let mut w = claude(&agent_screen());
    w.tick(SEEN).unwrap();
    w.tick(DUE - 1).unwrap();
    assert_eq!(reads(&w), 1, "nothing moved and nothing is due");
    assert_eq!(
        w.tick(DUE).unwrap(),
        [acted(input(&["continue", "\r"]), false), tick(&["claude"])]
    );
}

#[test]
fn types_continue_and_confirms_it_before_enter() {
    let mut w = claude(&agent_screen());
    act(&mut w);
    let read = Call::Read(CLAUDE.to_owned());
    let text = Call::Send(Keys::Text("continue".to_owned()));
    let expected = [
        read.clone(),
        text,
        Call::Pause,
        read,
        Call::Send(Keys::Enter),
    ];
    assert_eq!(w.cli.calls, expected);
}

#[test]
fn user_typing_before_enter_aborts_the_submit() {
    let mut w = claude(&agent_screen());
    w.cli.interject = Some(" hello".to_owned());
    assert_eq!(act(&mut w), [tick(&["claude"])]);
    assert_eq!(sends(&w), [&Keys::Text("continue".to_owned())]);
}

#[test]
fn types_only_into_an_empty_input_box() {
    for screen in [
        agent_screen().replace("\n❯\n", "\n❯ my draft\n"),
        "API Error: 529".to_owned(),
    ] {
        let mut w = claude(&screen);
        assert_eq!(act(&mut w), [tick(&["claude"])], "{screen}");
        assert!(sends(&w).is_empty());
    }
}

#[test]
fn orca_draft_blocks_typing_and_pauses_the_engine() {
    let mut w = claude(&agent_screen());
    w.cli.draft = true;
    act(&mut w);
    assert!(sends(&w).is_empty());
    assert_eq!(
        w.tracked[CLAUDE].engine.as_ref().unwrap().user_inputs,
        [SEEN, DUE]
    );
}

#[test]
fn bare_enter_submits_only_the_text_we_typed_before() {
    let mut w = claude(&agent_screen());
    w.tick(T0).unwrap();
    w.cli.typed.insert(CLAUDE.to_owned(), "continue".to_owned());
    let screen = w.cli.read_screen(CLAUDE).unwrap();
    w.cli.calls.clear();
    let mut events = Vec::new();
    w.execute(CLAUDE, "claude", &screen, input(&["\r"]), &mut events);
    assert!(
        w.cli.calls.is_empty() && events.is_empty(),
        "nothing of ours to submit"
    );

    w.tracked.get_mut(CLAUDE).unwrap().typed = Some("continue".to_owned());
    w.execute(CLAUDE, "claude", &screen, input(&["\r"]), &mut events);
    assert_eq!(sends(&w), [&Keys::Enter]);
    assert_eq!(events, [acted(input(&["\r"]), false)]);
}

#[test]
fn dry_run_reports_without_typing() {
    let mut w = claude(&agent_screen());
    w.dry_run = true;
    assert_eq!(act(&mut w)[0], acted(input(&["continue", "\r"]), true));
    assert!(sends(&w).is_empty());
}

#[test]
fn never_types_into_a_shell_and_detaches_an_exited_agent() {
    let mut w = claude(&exited_screen());
    assert_eq!(act(&mut w), [tick(&[])]);
    assert!(sends(&w).is_empty());

    let mut w = claude(&agent_screen());
    assert_eq!(w.tick(T0).unwrap()[0], attached(CLAUDE, "claude"));
    w.cli.screens.insert(CLAUDE.to_owned(), exited_screen());
    w.cli.terminals[0].last_output_at = T0 + 1;
    assert_eq!(w.tick(T0 + 2).unwrap(), [detached(CLAUDE), tick(&[])]);
}

#[test]
fn active_agents_cover_every_listed_agent_terminal_but_exited_ones() {
    let read_only = TerminalInfo {
        writable: false,
        ..terminal("term_codex", Some("codex"))
    };
    let mut w = watcher(
        vec![terminal(CLAUDE, Some("claude")), read_only],
        &exited_screen(),
        Vec::new(),
    );
    assert_eq!(w.tick(T0).unwrap(), [tick(&["codex"])]);

    let pin = TerminalPin {
        handle: "term_probe".to_owned(),
        agent: "claude".to_owned(),
    };
    let mut w = watcher(
        vec![
            terminal("term_probe", None),
            terminal("term_codex", Some("codex")),
        ],
        "idle",
        vec![pin],
    );
    w.cli.fail_reads = true;
    assert_eq!(
        w.tick(T0).unwrap(),
        [tick(&["claude", "codex"])],
        "unread and unpinned count"
    );
}

#[test]
fn failing_reads_back_off() {
    let mut w = claude("idle");
    w.cli.fail_reads = true;
    for second in 0..30 {
        w.tick(T0 + second * 1_000).unwrap();
    }
    assert_eq!(reads(&w), 4, "read at 0, 2, 6 and 14 s");
}

#[test]
fn engine_survives_a_flickering_list() {
    let mut w = claude("idle");
    w.tick(T0).unwrap();
    let listed = w.cli.terminals.remove(0);
    for cycle in 1..=u64::from(MAX_MISSED_CYCLES) {
        assert_eq!(w.tick(T0 + cycle).unwrap(), [tick(&["claude"])]);
    }
    w.cli.terminals.push(listed);
    assert_eq!(
        w.tick(T0 + 10).unwrap(),
        [tick(&["claude"])],
        "no re-attach"
    );
    assert_eq!(
        w.tracked[CLAUDE].engine.as_ref().unwrap().steps,
        1,
        "same engine"
    );

    w.cli.terminals.clear();
    for cycle in 1..=u64::from(MAX_MISSED_CYCLES) {
        w.tick(T0 + 10 + cycle).unwrap();
    }
    assert_eq!(w.tick(T0 + 20).unwrap(), [detached(CLAUDE), tick(&[])]);
}

#[test]
fn real_engine_continues_an_overloaded_claude() {
    let mut policy = harn_core::Policy {
        settle_ms: 1_000,
        ..harn_core::Policy::default()
    };
    policy.reactions.overloaded.delay_secs = 1;
    let agents = CoreAgents {
        profiles: Profile::builtins(),
        policy,
    };
    let terminals = vec![terminal(CLAUDE, Some("claude"))];
    let screens = HashMap::from([(CLAUDE.to_owned(), agent_screen())]);
    let mut w = Watcher::new(
        FakeCli {
            terminals,
            screens,
            ..FakeCli::default()
        },
        agents,
        false,
        Vec::new(),
    );
    let mut now = T0 + 2_000;
    while now < T0 + 60_000 && !w.cli.calls.contains(&Call::Send(Keys::Enter)) {
        w.tick(now).unwrap();
        now += 500;
    }
    let sent: Vec<&Call> = w
        .cli
        .calls
        .iter()
        .filter(|c| matches!(c, Call::Send(_)))
        .collect();
    let text = Call::Send(Keys::Text("continue".to_owned()));
    assert_eq!(sent, [&text, &Call::Send(Keys::Enter)]);
}
