//! One poll cycle over Orca's terminals: pick the agent terminals, feed changed screens to their
//! engines and type what they decide, only into the agent's empty input box, pressing Enter only
//! once a fresh read shows exactly our text there.

use std::collections::BTreeMap;

use harn_core::{Action, Engine, Policy, Profile, Snapshot};
use tracing::{info, warn};

use crate::cli::{CliError, Keys, OrcaCli, Screen, TerminalInfo};
use crate::screen::{InputRow, input_row, shows_shell_prompt};
use crate::{Event, TerminalPin};

/// The submit chunk that `--enter` sends, so no raw control character goes through argv.
const ENTER: &str = "\r";
/// List cycles a terminal may be missing before its engine (attempts, backoff, hourly cap) is dropped.
const MAX_MISSED_CYCLES: u32 = 3;
/// Reads of the input box before Enter; the later ones allow for the agent's render lag.
const CONFIRM_READS: u32 = 3;
/// Delay before re-reading a terminal whose read failed; doubles per failure up to the max.
const READ_RETRY_BASE_MS: u64 = 2_000;
const READ_RETRY_MAX_MS: u64 = 60_000;

pub(crate) trait AgentEngine {
    fn step(&mut self, snap: &Snapshot, now_ms: u64) -> Vec<Action>;
    fn next_deadline_ms(&self) -> Option<u64>;
    fn note_user_input(&mut self, now_ms: u64);
}

impl AgentEngine for Engine {
    fn step(&mut self, snap: &Snapshot, now_ms: u64) -> Vec<Action> {
        Engine::step(self, snap, now_ms)
    }

    fn next_deadline_ms(&self) -> Option<u64> {
        Engine::next_deadline_ms(self)
    }

    fn note_user_input(&mut self, now_ms: u64) {
        Engine::note_user_input(self, now_ms);
    }
}

/// Maps agent names to profiles and builds their engines; tests substitute scripted engines.
pub(crate) trait Agents {
    type Engine: AgentEngine;
    /// Id of the profile that handles `agent` (an Orca agent identity, profile id or alias).
    fn profile_id(&self, agent: &str) -> Option<String>;
    fn engine(&self, profile_id: &str) -> Option<Self::Engine>;
    fn input_row(&self, profile_id: &str, screen: &str) -> InputRow;
}

pub(crate) struct CoreAgents {
    pub profiles: Vec<Profile>,
    pub policy: Policy,
}

impl Agents for CoreAgents {
    type Engine = Engine;

    fn profile_id(&self, agent: &str) -> Option<String> {
        Some(self.profiles.iter().find(|p| p.matches(agent))?.id.clone())
    }

    fn engine(&self, profile_id: &str) -> Option<Engine> {
        let profile = self.profiles.iter().find(|p| p.id == profile_id)?;
        Some(Engine::new(profile.clone(), self.policy.clone()))
    }

    fn input_row(&self, profile_id: &str, screen: &str) -> InputRow {
        let profile = self.profiles.iter().find(|p| p.id == profile_id);
        profile.map_or(InputRow::Missing, |p| input_row(p, screen))
    }
}

struct Tracked<E> {
    profile_id: String,
    /// Present while the agent looks alive; created on the first screen that is not a shell.
    engine: Option<E>,
    /// The screen showed a shell prompt: the agent is gone even if Orca still names it.
    exited: bool,
    missed: u32,
    /// `lastOutputAt` as of the last successful read.
    read_output_at: Option<u64>,
    read_failures: u32,
    retry_at_ms: u64,
    /// Our text last typed into the input box; a later bare Enter may only submit exactly this.
    typed: Option<String>,
}

pub(crate) struct Watcher<C, A: Agents> {
    cli: C,
    agents: A,
    dry_run: bool,
    pinned: Vec<TerminalPin>,
    tracked: BTreeMap<String, Tracked<A::Engine>>,
}

/// Text typed into the input box, as opposed to keys like Esc or Enter.
fn is_text(chunk: &str) -> bool {
    !chunk.is_empty() && !chunk.chars().any(char::is_control)
}

impl<C: OrcaCli, A: Agents> Watcher<C, A> {
    pub(crate) fn new(cli: C, agents: A, dry_run: bool, pinned: Vec<TerminalPin>) -> Self {
        let tracked = BTreeMap::new();
        Self {
            cli,
            agents,
            dry_run,
            pinned,
            tracked,
        }
    }

    /// One poll cycle. Fails only if the terminal list cannot be fetched; per-terminal failures
    /// are logged and retried later.
    pub(crate) fn tick(&mut self, now_ms: u64) -> Result<Vec<Event>, CliError> {
        let terminals = self.cli.list()?;
        let watched: Vec<(&TerminalInfo, String)> = terminals
            .iter()
            .filter(|t| t.connected && t.writable)
            .filter_map(|t| Some((t, self.profile_of(t, true)?)))
            .collect();
        let mut events = Vec::new();
        self.sweep(&watched, &mut events);
        for (terminal, profile_id) in &watched {
            let Some((screen, actions)) = self.observe(terminal, profile_id, now_ms, &mut events)
            else {
                continue;
            };
            for action in actions {
                self.execute(&terminal.handle, profile_id, &screen, action, &mut events);
            }
        }
        events.push(Event::Tick {
            active_agents: self.active_agents(&terminals),
        });
        Ok(events)
    }

    /// The profile for a terminal's agent: its pin, else Orca's identity. When `watching` with pins,
    /// only pinned terminals count.
    fn profile_of(&self, terminal: &TerminalInfo, watching: bool) -> Option<String> {
        let pin = self.pinned.iter().find(|p| p.handle == terminal.handle);
        let agent = match pin {
            Some(pin) => &pin.agent,
            None if watching && !self.pinned.is_empty() => return None,
            None => terminal.agent_identity.as_deref()?,
        };
        self.agents.profile_id(agent)
    }

    /// Forgets terminals that left the watched set: at once if their agent changed, after
    /// `MAX_MISSED_CYCLES` if merely missing, so a flickering list keeps the engine and its counters.
    fn sweep(&mut self, watched: &[(&TerminalInfo, String)], events: &mut Vec<Event>) {
        self.tracked.retain(|handle, tracked| {
            let keep = match watched.iter().find(|(t, _)| t.handle == *handle) {
                Some((_, id)) => {
                    tracked.missed = 0;
                    *id == tracked.profile_id
                }
                None => {
                    tracked.missed += 1;
                    tracked.missed <= MAX_MISSED_CYCLES
                }
            };
            if !keep && tracked.engine.is_some() {
                info!(%handle, agent = %tracked.profile_id, "agent terminal gone; detached");
                events.push(Event::Detached {
                    handle: handle.clone(),
                });
            }
            keep
        });
    }

    /// The screen, if output moved since the last read or the engine is due, minding read backoff.
    fn read_if_needed(
        &mut self,
        terminal: &TerminalInfo,
        profile_id: &str,
        now_ms: u64,
    ) -> Option<Screen> {
        let handle = &terminal.handle;
        let tracked = self
            .tracked
            .entry(handle.clone())
            .or_insert_with(|| Tracked {
                profile_id: profile_id.to_owned(),
                engine: None,
                exited: false,
                missed: 0,
                read_output_at: None,
                read_failures: 0,
                retry_at_ms: 0,
                typed: None,
            });
        let due = tracked
            .engine
            .as_ref()
            .and_then(AgentEngine::next_deadline_ms);
        let moved = tracked.read_output_at != Some(terminal.last_output_at);
        if now_ms < tracked.retry_at_ms || !(moved || due.is_some_and(|due| due <= now_ms)) {
            return None;
        }
        match self.cli.read_screen(handle) {
            Ok(screen) => {
                tracked.read_output_at = Some(terminal.last_output_at);
                tracked.read_failures = 0;
                tracked.retry_at_ms = 0;
                Some(screen)
            }
            Err(error) => {
                let doublings = tracked.read_failures.min(5);
                tracked.read_failures += 1;
                let retry_in_ms = (READ_RETRY_BASE_MS << doublings).min(READ_RETRY_MAX_MS);
                tracked.retry_at_ms = now_ms + retry_in_ms;
                warn!(%handle, agent = %profile_id, %error, retry_in_ms, "cannot read terminal screen");
                None
            }
        }
    }

    /// Attaches, detaches or steps the engine depending on what a fresh screen shows.
    fn observe(
        &mut self,
        terminal: &TerminalInfo,
        profile_id: &str,
        now_ms: u64,
        events: &mut Vec<Event>,
    ) -> Option<(Screen, Vec<Action>)> {
        let handle = &terminal.handle;
        let screen = self.read_if_needed(terminal, profile_id, now_ms)?;
        let tracked = self.tracked.get_mut(handle)?;
        tracked.exited = shows_shell_prompt(&screen.text);
        if tracked.exited {
            if tracked.engine.take().is_some() {
                info!(%handle, agent = %profile_id, "shell prompt on screen, agent exited; detached");
                events.push(Event::Detached {
                    handle: handle.clone(),
                });
            }
            return None;
        }
        if tracked.engine.is_none() {
            tracked.engine = Some(self.agents.engine(profile_id)?);
            info!(%handle, agent = %profile_id, "watching agent terminal");
            let agent = profile_id.to_owned();
            events.push(Event::Attached {
                handle: handle.clone(),
                agent,
            });
        }
        let engine = tracked.engine.as_mut()?;
        if screen.has_draft {
            engine.note_user_input(now_ms);
        }
        let snap = Snapshot {
            text: screen.text.clone(),
            last_output_ms: terminal.last_output_at,
        };
        let actions = engine.step(&snap, now_ms);
        Some((screen, actions))
    }

    /// Agents that may be running, which gates replacing their binaries: every listed terminal whose
    /// agent is known (watched or not), plus tracked ones briefly missing from the list, except
    /// terminals whose screen shows the agent exited to a shell.
    fn active_agents(&self, terminals: &[TerminalInfo]) -> Vec<String> {
        let exited = |handle: &str| self.tracked.get(handle).is_some_and(|t| t.exited);
        let listed = terminals
            .iter()
            .filter(|t| !exited(&t.handle))
            .filter_map(|t| self.profile_of(t, false));
        let tracked = self
            .tracked
            .values()
            .filter(|t| !t.exited)
            .map(|t| t.profile_id.clone());
        let mut ids: Vec<String> = listed.chain(tracked).collect();
        ids.sort();
        ids.dedup();
        ids
    }

    fn execute(
        &mut self,
        handle: &str,
        agent: &str,
        screen: &Screen,
        action: Action,
        events: &mut Vec<Event>,
    ) {
        if let Action::Input {
            chunks,
            gap_ms,
            trigger,
            attempt,
        } = &action
        {
            let typed = self
                .check_before_typing(agent, screen, chunks)
                .and_then(|()| {
                    if self.dry_run {
                        Ok(())
                    } else {
                        self.type_chunks(handle, agent, chunks, *gap_ms)
                    }
                });
            if let Err(reason) = typed {
                warn!(handle, agent, ?trigger, attempt, %reason, "not typing");
                return;
            }
            let dry_run = self.dry_run;
            info!(
                handle,
                agent,
                ?trigger,
                attempt,
                ?chunks,
                dry_run,
                "typed into terminal"
            );
        } else if let Action::Notify { trigger, message } = &action {
            warn!(handle, agent, ?trigger, "{message}");
        }
        let (handle, agent, dry_run) = (handle.to_owned(), agent.to_owned(), self.dry_run);
        events.push(Event::Acted {
            handle,
            agent,
            action,
            dry_run,
        });
    }

    /// Refuses before any key is sent: the user is drafting in Orca, or the action types text while
    /// the agent's input box is missing or already holds something.
    fn check_before_typing(
        &self,
        agent: &str,
        screen: &Screen,
        chunks: &[String],
    ) -> Result<(), String> {
        if screen.has_draft {
            return Err("the user is composing a prompt in Orca".into());
        }
        if !chunks.iter().any(|c| is_text(c)) {
            return Ok(());
        }
        match self.agents.input_row(agent, &screen.text) {
            InputRow::Empty => Ok(()),
            InputRow::Missing => Err("the agent's input box is not on screen".into()),
            InputRow::Holds(_) => Err("the agent's input box already holds text".into()),
        }
    }

    /// Sends chunks in order with `gap_ms` between them. Enter submits only the text typed just
    /// before it, or, for a bare Enter (the engine re-submitting), the text we typed earlier.
    fn type_chunks(
        &mut self,
        handle: &str,
        agent: &str,
        chunks: &[String],
        gap_ms: u64,
    ) -> Result<(), String> {
        let mut typed = String::new();
        for (i, chunk) in chunks.iter().filter(|c| !c.is_empty()).enumerate() {
            if i > 0 {
                self.cli.pause(gap_ms);
            }
            if chunk != ENTER {
                self.cli
                    .send(handle, &Keys::Text(chunk.clone()))
                    .map_err(|e| e.to_string())?;
                if is_text(chunk) {
                    typed.push_str(chunk);
                    if let Some(tracked) = self.tracked.get_mut(handle) {
                        tracked.typed = Some(typed.trim().to_owned());
                    }
                }
                continue;
            }
            let earlier = self.tracked.get(handle).and_then(|t| t.typed.clone());
            let expected = if typed.is_empty() {
                earlier
            } else {
                Some(typed.trim().to_owned())
            };
            self.confirm_input(
                handle,
                agent,
                &expected.ok_or("no text of ours to submit")?,
                gap_ms,
            )?;
            self.cli
                .send(handle, &Keys::Enter)
                .map_err(|e| e.to_string())?;
            typed.clear();
        }
        Ok(())
    }

    /// Re-reads until the input box shows exactly `expected`. An empty box or a prefix of our text is
    /// render lag and is read again; anything else (the user typing, a dialog, a shell) aborts.
    fn confirm_input(
        &mut self,
        handle: &str,
        agent: &str,
        expected: &str,
        gap_ms: u64,
    ) -> Result<(), String> {
        for read in 0..CONFIRM_READS {
            if read > 0 {
                self.cli.pause(gap_ms);
            }
            let screen = self.cli.read_screen(handle).map_err(|e| e.to_string())?;
            if screen.has_draft || shows_shell_prompt(&screen.text) {
                return Err("the user or a shell took over the terminal".into());
            }
            match self.agents.input_row(agent, &screen.text) {
                InputRow::Holds(text) if text == expected => return Ok(()),
                InputRow::Empty => {}
                InputRow::Holds(text) if expected.starts_with(&text) => {}
                _ => return Err("the input box holds other text or is gone".into()),
            }
        }
        Err("the typed text never showed up in the input box".into())
    }
}

#[cfg(test)]
mod tests;
