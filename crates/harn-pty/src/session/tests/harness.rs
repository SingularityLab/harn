//! The test side: a command in a real PTY behind a scripted terminal.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use harn_core::{Policy, Profile, Reaction, ReactionKind, Reactions};
use portable_pty::{CommandBuilder, PtySize};

use crate::automation::Automation;
use crate::command;
use crate::session::{Pty, Terminal, run};

pub(super) const SIZE: PtySize = PtySize {
    rows: 30,
    cols: 120,
    pixel_width: 0,
    pixel_height: 0,
};
/// What [`Step::Resize`] switches the terminal to.
pub(super) const RESIZED: PtySize = PtySize {
    rows: 25,
    cols: 100,
    ..SIZE
};
/// The watchdog kills the agent after this long.
const SESSION_TIMEOUT: Duration = Duration::from_secs(60);
const TEST_PROFILE: &str = r#"
id = "fake"
name = "Fake agent"
gap_ms = 50

[signatures]
working = ["esc to interrupt"]
overloaded = ["overloaded_error", "API Error: 529"]
user_echo = ['^❯(?:\s|$)']
empty_input = ['^❯\s*$']
"#;

/// What the scripted terminal does once its prompt has appeared.
pub(super) enum Step {
    Type(&'static str),
    Resize,
}

pub(super) struct Finished {
    pub(super) code: i32,
    pub(super) output: String,
}

/// libtest arguments that run exactly the test at `test_path`, printing straight to the PTY.
pub(super) fn fake_args(test_path: &str) -> Vec<OsString> {
    let args = [
        "--exact",
        test_path,
        "--nocapture",
        "--quiet",
        "--test-threads=1",
    ];
    args.map(OsString::from).to_vec()
}

pub(super) fn child_command(test_path: &str, env: &[(&str, &str)]) -> CommandBuilder {
    let exe = std::env::current_exe().unwrap();
    let mut cmd = command::build(exe.as_os_str(), &fake_args(test_path)).unwrap();
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd
}

/// Runs `cmd` to the end; cursor-position queries are answered by the session, the rest by
/// `script`.
pub(super) fn run_headless(
    cmd: CommandBuilder,
    automation: Option<Automation>,
    script: Vec<(&'static str, Step)>,
) -> Finished {
    let pty = Pty::spawn(cmd, SIZE, OsStr::new("test agent")).unwrap();
    let mut killer = pty.child.clone_killer();
    let (disarm, disarmed) = mpsc::channel::<()>();
    thread::spawn(move || {
        if disarmed.recv_timeout(SESSION_TIMEOUT) == Err(RecvTimeoutError::Timeout) {
            killer.kill().ok();
        }
    });
    let (keys_tx, keys) = mpsc::channel();
    let (capture, captured) = mpsc::channel();
    let (resize, resized) = mpsc::channel();
    let mut size = SIZE;
    let terminal = Terminal {
        input: Box::new(ChannelInput {
            keys,
            pending: Vec::new(),
        }),
        output: Box::new(ScriptedOutput {
            seen: Vec::new(),
            script,
            keys: keys_tx,
            resize,
            capture,
        }),
        size: Box::new(move || {
            if resized.try_recv().is_ok() {
                size = RESIZED;
            }
            Some(size)
        }),
        answer_cursor_queries: true,
    };
    let code = run(pty, automation, terminal).unwrap();
    drop(disarm);
    let output = captured.try_iter().collect::<Vec<_>>().concat();
    Finished {
        code,
        output: String::from_utf8_lossy(&output).into_owned(),
    }
}

/// The test profile with default policy, except that an overload is answered once it settles.
pub(super) fn automation() -> Automation {
    let defaults = Policy::default();
    let overloaded = Reaction {
        action: ReactionKind::Continue,
        delay_secs: 0,
        max_delay_secs: 0,
        ..defaults.reactions.overloaded.clone()
    };
    let policy = Policy {
        settle_ms: 300,
        user_grace_ms: 0,
        reactions: Reactions {
            overloaded,
            ..defaults.reactions.clone()
        },
        ..defaults
    };
    Automation {
        profile: Profile::from_toml(TEST_PROFILE).unwrap(),
        policy,
        dry_run: false,
    }
}

struct ScriptedOutput {
    seen: Vec<u8>,
    script: Vec<(&'static str, Step)>,
    keys: Sender<Vec<u8>>,
    resize: Sender<()>,
    capture: Sender<Vec<u8>>,
}

impl Write for ScriptedOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.seen.extend_from_slice(buf);
        self.capture.send(buf.to_vec()).ok();
        let seen = String::from_utf8_lossy(&self.seen).into_owned();
        let (shown, waiting): (Vec<_>, _) = std::mem::take(&mut self.script)
            .into_iter()
            .partition(|(prompt, _)| seen.contains(prompt));
        self.script = waiting;
        for (_, step) in shown {
            match step {
                Step::Type(keys) => self.keys.send(keys.as_bytes().to_vec()).ok(),
                Step::Resize => self.resize.send(()).ok(),
            };
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct ChannelInput {
    keys: Receiver<Vec<u8>>,
    pending: Vec<u8>,
}

impl Read for ChannelInput {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pending.is_empty() {
            match self.keys.recv() {
                Ok(bytes) => self.pending = bytes,
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}
