//! A running agent: the pump threads plus the loop that ties output, input and automation
//! together. Everything outside comes in through [`Terminal`], so tests can run it headless.

use std::ffi::OsStr;
use std::fmt::Display;
use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use harn_core::Action;
use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize};

use crate::PtyError;
use crate::automation::{Automation, Driver, EngineFailed, Screen};
use crate::pump::{self, Event, OutputPump};
use crate::queue::InputQueue;

#[cfg(test)]
mod tests;

/// Engine step and terminal-size poll interval.
const TICK: Duration = Duration::from_millis(200);
/// Once the agent has exited, output this quiet means it has said everything...
const EXIT_QUIET: Duration = Duration::from_millis(100);
/// ...but a grandchild may keep the terminal open, so stop waiting after this long.
const EXIT_DRAIN_MAX: Duration = Duration::from_secs(2);
/// After closing the pseudo terminal, how long to wait for its output stream to end.
const CLOSE_WAIT: Duration = Duration::from_millis(500);

/// The caller's side of the session. Only the output pump writes to `output`: anything else
/// written there (a notification bell, say) could land inside one of the agent's escape sequences.
pub(crate) struct Terminal {
    pub(crate) input: Box<dyn Read + Send>,
    pub(crate) output: Box<dyn Write + Send>,
    /// Polled every tick; `None` = unknown, keep the current size.
    pub(crate) size: Box<dyn FnMut() -> Option<PtySize>>,
    /// No real terminal will answer the agent's cursor-position queries, so the session does.
    pub(crate) answer_cursor_queries: bool,
}

pub(crate) struct Pty {
    pub(crate) master: Box<dyn MasterPty + Send>,
    pub(crate) child: Box<dyn Child + Send + Sync>,
    pub(crate) size: PtySize,
}

impl Pty {
    pub(crate) fn spawn(
        cmd: CommandBuilder,
        size: PtySize,
        program: &OsStr,
    ) -> Result<Self, PtyError> {
        let pair = portable_pty::native_pty_system()
            .openpty(size)
            .map_err(pty_error("cannot open a pseudo terminal"))?;
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|err| PtyError::Spawn {
                program: program.to_string_lossy().into_owned(),
                reason: format!("{err:#}"),
            })?;
        // Our slave handle would keep the output open after the agent exits (Unix) and the pseudo
        // console alive after the master is dropped (Windows).
        drop(pair.slave);
        Ok(Self {
            master: pair.master,
            child,
            size,
        })
    }
}

/// Runs the session until the agent exits and returns its exit code.
pub(crate) fn run(
    pty: Pty,
    automation: Option<Automation>,
    terminal: Terminal,
) -> Result<i32, PtyError> {
    let mut killer = pty.child.clone_killer();
    match start(pty, automation, terminal) {
        Ok((session, events)) => session.run(&events),
        Err(err) => {
            if let Err(kill_err) = killer.kill() {
                tracing::warn!(%kill_err, "cannot stop the agent after a failed start");
            }
            Err(err)
        }
    }
}

fn start(
    pty: Pty,
    automation: Option<Automation>,
    terminal: Terminal,
) -> Result<(Session, Receiver<Event>), PtyError> {
    let Pty {
        master,
        child,
        size,
    } = pty;
    let reader = master
        .try_clone_reader()
        .map_err(pty_error("cannot read from the pseudo terminal"))?;
    let writer = master
        .take_writer()
        .map_err(pty_error("cannot write to the pseudo terminal"))?;
    let (events_tx, events) = mpsc::channel();
    let (pty_input, writes) = mpsc::channel();
    spawn("harn-pty-writer", move || pump::pump_writes(writer, writes))?;
    let output = OutputPump {
        pty: reader,
        terminal: terminal.output,
        events: events_tx.clone(),
        copy_output: automation.is_some(),
        answer_queries: terminal.answer_cursor_queries.then(|| pty_input.clone()),
    };
    spawn("harn-pty-output", move || output.run())?;
    let (keys, key_events) = (pty_input.clone(), events_tx.clone());
    let input = terminal.input;
    spawn("harn-pty-input", move || {
        pump::pump_input(input, keys, key_events)
    })?;
    spawn("harn-pty-wait", move || pump::wait_child(child, events_tx))?;
    let (driver, dry_run) = driver(automation, size);
    let session = Session {
        master: Some(master),
        size,
        poll_size: terminal.size,
        driver,
        dry_run,
        queue: InputQueue::default(),
        pty_input,
        next_tick: Instant::now(),
        output_closed: false,
    };
    Ok((session, events))
}

fn driver(automation: Option<Automation>, size: PtySize) -> (Option<Driver>, bool) {
    let Some(Automation {
        profile,
        policy,
        dry_run,
    }) = automation
    else {
        return (None, false);
    };
    tracing::info!(profile = %profile.id, dry_run, "auto-continue is on");
    let screen = Screen::new(size, wall_ms());
    (Some(Driver::new(profile, policy, screen)), dry_run)
}

/// Owned by the calling thread; the only place that sees the screen, the engine and the clock.
struct Session {
    /// `None` once closed, which on Windows is what ends the output stream.
    master: Option<Box<dyn MasterPty + Send>>,
    size: PtySize,
    poll_size: Box<dyn FnMut() -> Option<PtySize>>,
    /// `None` for plain passthrough, or after the engine failed.
    driver: Option<Driver>,
    dry_run: bool,
    queue: InputQueue,
    pty_input: Sender<Vec<u8>>,
    next_tick: Instant,
    output_closed: bool,
}

impl Session {
    fn run(mut self, events: &Receiver<Event>) -> Result<i32, PtyError> {
        let status = self.until_exit(events)?;
        self.finish(events);
        if let Some(signal) = status.signal() {
            tracing::info!(signal, "agent was terminated by a signal");
        }
        Ok(exit_code(&status))
    }

    fn until_exit(&mut self, events: &Receiver<Event>) -> Result<ExitStatus, PtyError> {
        loop {
            let now = Instant::now();
            self.on_time(now);
            let event = match events.recv_timeout(self.time_to_next(now)) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(PtyError::Other(
                        "the session threads stopped before the agent exited".into(),
                    ));
                }
            };
            if let Some(status) = self.handle(event) {
                return status.map_err(|source| PtyError::Io {
                    context: "cannot read the agent's exit status",
                    source,
                });
            }
        }
    }

    fn handle(&mut self, event: Event) -> Option<io::Result<ExitStatus>> {
        match event {
            Event::Output(bytes) => self.screen_output(&bytes),
            Event::UserInput => self.user_input(),
            Event::OutputClosed => {
                tracing::debug!("agent output ended");
                self.output_closed = true;
            }
            Event::Exited(status) => return Some(status),
        }
        None
    }

    fn on_time(&mut self, now: Instant) {
        if now >= self.next_tick {
            self.next_tick = now + TICK;
            self.poll_resize();
            self.step_automation(now);
        }
        self.flush_due(now);
    }

    fn time_to_next(&self, now: Instant) -> Duration {
        let next = self
            .queue
            .next_due()
            .map_or(self.next_tick, |due| due.min(self.next_tick));
        next.saturating_duration_since(now)
    }

    fn poll_resize(&mut self) {
        let Some(size) = (self.poll_size)() else {
            return;
        };
        let Some(master) = &self.master else {
            return;
        };
        if size == self.size {
            return;
        }
        if let Err(err) = master.resize(size) {
            tracing::warn!(%err, "cannot resize the pseudo terminal");
            return;
        }
        tracing::debug!(rows = size.rows, cols = size.cols, "terminal resized");
        self.size = size;
        let Some(driver) = &mut self.driver else {
            return;
        };
        if let Err(EngineFailed) = driver.resize(size) {
            self.driver = None;
        }
    }

    fn screen_output(&mut self, bytes: &[u8]) {
        let Some(driver) = &mut self.driver else {
            return;
        };
        if let Err(EngineFailed) = driver.output(bytes, wall_ms()) {
            self.driver = None;
        }
    }

    fn step_automation(&mut self, now: Instant) {
        let Some(driver) = &mut self.driver else {
            return;
        };
        match driver.step(wall_ms()) {
            Ok(actions) => actions
                .into_iter()
                .for_each(|action| self.execute(action, now)),
            Err(EngineFailed) => self.driver = None,
        }
    }

    fn user_input(&mut self) {
        if !self.queue.is_empty() {
            // The pending Enter would submit the user's keys glued to our text.
            tracing::info!("the user is typing; dropping the rest of the injected input");
            self.queue.clear();
        }
        let Some(driver) = &mut self.driver else {
            return;
        };
        if let Err(EngineFailed) = driver.user_input(wall_ms()) {
            self.driver = None;
        }
    }

    fn execute(&mut self, action: Action, now: Instant) {
        match action {
            Action::Input {
                chunks,
                gap_ms,
                trigger,
                attempt,
            } => {
                if self.dry_run {
                    tracing::info!(?trigger, attempt, ?chunks, "dry run: would type");
                    return;
                }
                tracing::info!(?trigger, attempt, ?chunks, "typing into the agent");
                let chunks = chunks.into_iter().map(String::into_bytes);
                self.queue.push(chunks, Duration::from_millis(gap_ms), now);
            }
            Action::Notify { trigger, message } => {
                tracing::warn!(?trigger, %message, "the agent needs attention");
            }
        }
    }

    fn flush_due(&mut self, now: Instant) {
        while let Some(chunk) = self.queue.pop_due(now) {
            if self.pty_input.send(chunk).is_err() {
                tracing::warn!("agent input is closed; dropping injected input");
                self.queue.clear();
                return;
            }
        }
    }

    /// Lets the agent's last output reach the terminal, then closes the pseudo terminal.
    fn finish(&mut self, events: &Receiver<Event>) {
        self.queue.clear();
        if !self.output_closed {
            self.output_closed = await_output_end(events, EXIT_QUIET, EXIT_DRAIN_MAX);
        }
        // ConPTY keeps its output open until the pseudo console is closed; closing it flushes the
        // last frame and ends the stream.
        self.master = None;
        if !self.output_closed && !await_output_end(events, CLOSE_WAIT, CLOSE_WAIT) {
            tracing::debug!("agent output still open after exit; not waiting any longer");
        }
    }
}

/// True if the agent's output ended. Gives up once it has been quiet for `quiet`, or after `max`.
fn await_output_end(events: &Receiver<Event>, quiet: Duration, max: Duration) -> bool {
    let deadline = Instant::now() + max;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        match events.recv_timeout(quiet.min(left)) {
            Ok(Event::OutputClosed) => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
}

/// Windows exit codes are u32 (0xC000013A after Ctrl+C); keep the bit pattern, which is what the
/// OS does again when the caller hands it to `std::process::exit`.
fn exit_code(status: &ExitStatus) -> i32 {
    i32::from_ne_bytes(status.exit_code().to_ne_bytes())
}

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

fn spawn(name: &str, body: impl FnOnce() + Send + 'static) -> Result<(), PtyError> {
    thread::Builder::new()
        .name(name.into())
        .spawn(body)
        .map(drop)
        .map_err(|source| PtyError::Io {
            context: "cannot start a session thread",
            source,
        })
}

fn pty_error<E: Display>(context: &'static str) -> impl FnOnce(E) -> PtyError {
    move |err| PtyError::Pty {
        context,
        reason: format!("{err:#}"),
    }
}
