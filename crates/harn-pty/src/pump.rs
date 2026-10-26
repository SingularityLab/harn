//! Thread bodies. Each owns one blocking resource and talks to the session loop over channels.

use std::io::{self, Read, Write};
use std::sync::mpsc::{Receiver, Sender};
use std::thread;
use std::time::Duration;

use portable_pty::{Child, ExitStatus};

use crate::filter::Win32InputModeFilter;

const OUTPUT_BUF_BYTES: usize = 16 * 1024;
const INPUT_BUF_BYTES: usize = 8 * 1024;
/// DSR "report cursor position". ConPTY sends it on start-up and blocks until it is answered.
const CURSOR_QUERY: &[u8] = b"\x1b[6n";
const CURSOR_REPLY: &[u8] = b"\x1b[1;1R";
/// Before reading again after a transient error, so a persistent one cannot spin a core.
const RETRY_PAUSE: Duration = Duration::from_millis(10);

pub(crate) enum Event {
    /// A copy of the agent's output, for the screen model.
    Output(Vec<u8>),
    OutputClosed,
    UserInput,
    Exited(io::Result<ExitStatus>),
}

/// Agent output → the caller's terminal, as fast as it arrives, plus a copy for the session loop.
pub(crate) struct OutputPump {
    pub(crate) pty: Box<dyn Read + Send>,
    pub(crate) terminal: Box<dyn Write + Send>,
    pub(crate) events: Sender<Event>,
    pub(crate) copy_output: bool,
    /// Set when no real terminal will answer cursor-position queries (stdio is redirected).
    pub(crate) answer_queries: Option<Sender<Vec<u8>>>,
}

impl OutputPump {
    pub(crate) fn run(mut self) {
        let mut buf = vec![0; OUTPUT_BUF_BYTES];
        let mut filter = Win32InputModeFilter::default();
        let mut query_tail = Vec::new();
        let mut terminal_ok = true;
        while let Some(n) = read_some(self.pty.as_mut(), &mut buf, "agent output") {
            let chunk = &buf[..n];
            let shown = filter.filter(chunk);
            if !shown.is_empty() {
                terminal_ok = terminal_ok && write_terminal(self.terminal.as_mut(), &shown);
            }
            self.answer_cursor_queries(&mut query_tail, chunk);
            if self.copy_output && self.events.send(Event::Output(chunk.to_vec())).is_err() {
                break;
            }
        }
        let held = filter.finish();
        if terminal_ok && !held.is_empty() {
            write_terminal(self.terminal.as_mut(), &held);
        }
        // Fails only once the session loop is gone, and then nobody needs to know.
        self.events.send(Event::OutputClosed).ok();
    }

    fn answer_cursor_queries(&self, tail: &mut Vec<u8>, chunk: &[u8]) {
        let Some(input) = &self.answer_queries else {
            return;
        };
        for _ in 0..count_cursor_queries(tail, chunk) {
            if input.send(CURSOR_REPLY.to_vec()).is_err() {
                tracing::debug!("agent input closed; cursor query left unanswered");
                return;
            }
        }
    }
}

/// Cursor-position queries completed by `chunk`. `tail` carries the end of the stream between
/// calls, one byte short of a whole query: a split query is found, and none is counted twice.
fn count_cursor_queries(tail: &mut Vec<u8>, chunk: &[u8]) -> usize {
    tail.extend_from_slice(chunk);
    let found = (tail.windows(CURSOR_QUERY.len()))
        .filter(|candidate| *candidate == CURSOR_QUERY)
        .count();
    tail.drain(..tail.len().saturating_sub(CURSOR_QUERY.len() - 1));
    found
}

/// The caller's keystrokes → the agent, and a heads-up to the session loop.
pub(crate) fn pump_input(
    mut terminal: Box<dyn Read + Send>,
    pty: Sender<Vec<u8>>,
    events: Sender<Event>,
) {
    let mut buf = vec![0; INPUT_BUF_BYTES];
    while let Some(n) = read_some(terminal.as_mut(), &mut buf, "terminal input") {
        if pty.send(buf[..n].to_vec()).is_err() || events.send(Event::UserInput).is_err() {
            return;
        }
    }
}

/// The single writer into the agent: user keystrokes and injected input share it, so chunks never
/// interleave mid-write.
pub(crate) fn pump_writes(mut pty: Box<dyn Write + Send>, chunks: Receiver<Vec<u8>>) {
    for chunk in chunks {
        if let Err(err) = pty.write_all(&chunk).and_then(|()| pty.flush()) {
            tracing::debug!(%err, "agent input closed");
            return;
        }
    }
}

pub(crate) fn wait_child(mut child: Box<dyn Child + Send + Sync>, events: Sender<Event>) {
    let status = child.wait();
    // Fails only once the session loop is gone, and then nobody needs to know.
    events.send(Event::Exited(status)).ok();
}

/// `None` once the stream is over: end of stream or a fatal read error (logged).
fn read_some(src: &mut dyn Read, buf: &mut [u8], what: &str) -> Option<usize> {
    loop {
        match src.read(buf) {
            Ok(0) => {
                tracing::debug!("{what} reached end of stream");
                return None;
            }
            Ok(n) => return Some(n),
            Err(err) => thread::sleep(retry_after(&err, what)?),
        }
    }
}

/// How long to pause before reading again after `err`, or `None` if the stream is done.
fn retry_after(err: &io::Error, what: &str) -> Option<Duration> {
    match err.kind() {
        io::ErrorKind::Interrupted => Some(Duration::ZERO),
        // A non-blocking descriptor inherited from the parent.
        io::ErrorKind::WouldBlock => Some(RETRY_PAUSE),
        // std's Windows console stdin rejects an unpaired surrogate; only that input is lost.
        io::ErrorKind::InvalidData => {
            tracing::warn!(%err, "{what}: dropped input that could not be decoded");
            Some(RETRY_PAUSE)
        }
        // How ConPTY ends its output once the pseudo console is closed.
        io::ErrorKind::BrokenPipe => {
            tracing::debug!("{what} closed");
            None
        }
        _ => {
            tracing::warn!(%err, "cannot read {what}; no longer reading it");
            None
        }
    }
}

/// After the first failure the terminal is gone for good; keep draining the agent so it never
/// blocks on a full pipe, but stop writing.
fn write_terminal(terminal: &mut dyn Write, bytes: &[u8]) -> bool {
    match terminal.write_all(bytes).and_then(|()| terminal.flush()) {
        Ok(()) => true,
        Err(err) => {
            tracing::error!(%err, "cannot write to the terminal; discarding further agent output");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::ErrorKind;
    use std::sync::mpsc;

    use super::*;

    /// Each read returns the next scripted result, then end of stream.
    struct Scripted(VecDeque<Result<&'static [u8], ErrorKind>>);

    impl Read for Scripted {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let Some(read) = self.0.pop_front() else {
                return Ok(0);
            };
            let bytes = read?;
            buf[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        }
    }

    /// A non-blocking stdin inherited from the parent, or a console read std cannot decode, must not
    /// leave the keyboard dead for the session; a real failure ends the pump.
    #[test]
    fn input_survives_transient_read_errors() {
        let reads = [
            Err(ErrorKind::WouldBlock),
            Ok(&b"ab"[..]),
            Err(ErrorKind::InvalidData),
            Err(ErrorKind::Interrupted),
            Ok(&b"c"[..]),
            Err(ErrorKind::PermissionDenied),
            Ok(&b"never"[..]),
        ];
        let (pty, typed) = mpsc::channel();
        let (events, _heads_up) = mpsc::channel();
        pump_input(Box::new(Scripted(reads.into())), pty, events);
        assert_eq!(typed.iter().collect::<Vec<_>>().concat(), b"abc");
    }

    #[test]
    fn cursor_queries_are_answered_only_when_asked_also_when_split() {
        for answer in [true, false] {
            let reads = [Ok(&b"x\x1b["[..]), Ok(&b"6ny\x1b[6"[..]), Ok(&b"n"[..])];
            let (events, copies) = mpsc::channel();
            let (input, replies) = mpsc::channel();
            let pump = OutputPump {
                pty: Box::new(Scripted(reads.into())),
                terminal: Box::new(io::sink()),
                events,
                copy_output: true,
                answer_queries: answer.then_some(input),
            };
            pump.run();
            let copied: Vec<u8> = (copies.iter())
                .filter_map(|event| match event {
                    Event::Output(bytes) => Some(bytes),
                    _ => None,
                })
                .flatten()
                .collect();
            assert_eq!(copied, b"x\x1b[6ny\x1b[6n");
            assert_eq!(replies.iter().count(), if answer { 2 } else { 0 });
        }
    }
}
