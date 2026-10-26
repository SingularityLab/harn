//! The agent's rendered screen and the engine that watches it.

use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc::Sender;

use harn_core::{Action, Engine, Policy, Profile, Snapshot};
use portable_pty::PtySize;

/// What `run` was asked to automate.
#[derive(Debug)]
pub(crate) struct Automation {
    pub(crate) profile: Profile,
    pub(crate) policy: Policy,
    pub(crate) dry_run: bool,
    pub(crate) actions: Option<Sender<Action>>,
}

/// The agent's screen rebuilt from its raw output, and the engine reading it. Every call returns
/// `None` once vt100 or the engine panicked (logged); the caller then drops the driver.
pub(crate) struct Driver {
    parser: vt100::Parser,
    last_output_ms: u64,
    engine: Engine,
}

impl Driver {
    pub(crate) fn new(profile: Profile, policy: Policy, size: PtySize, now_ms: u64) -> Self {
        Self {
            // No scrollback: the engine only looks at what is on screen.
            parser: vt100::Parser::new(size.rows, size.cols, 0),
            last_output_ms: now_ms,
            engine: Engine::new(profile, policy),
        }
    }

    pub(crate) fn output(&mut self, bytes: &[u8], now_ms: u64) -> Option<()> {
        self.last_output_ms = now_ms;
        guarded("screen output", || self.parser.process(bytes))
    }

    pub(crate) fn resize(&mut self, size: PtySize) -> Option<()> {
        guarded("screen resize", || {
            self.parser.screen_mut().set_size(size.rows, size.cols);
        })
    }

    pub(crate) fn user_input(&mut self, now_ms: u64) -> Option<()> {
        guarded("note_user_input", || self.engine.note_user_input(now_ms))
    }

    pub(crate) fn step(&mut self, now_ms: u64) -> Option<Vec<Action>> {
        let snapshot = self.snapshot();
        guarded("step", || self.engine.step(&snapshot, now_ms))
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.parser.screen().contents(),
            last_output_ms: self.last_output_ms,
        }
    }
}

/// A bug in the engine or in vt100 (which panics on some wide-character/resize combinations) must
/// not take the agent down: only the automation stops. The driver is never used after a panic, so
/// the state it was left in does not matter.
fn guarded<T>(call: &str, f: impl FnOnce() -> T) -> Option<T> {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => Some(value),
        Err(payload) => {
            let panic = (payload.downcast_ref::<&str>().copied())
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("non-string panic payload");
            tracing::error!(
                call,
                panic,
                "auto-continue panicked; it is off for this session"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: PtySize = PtySize {
        rows: 6,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    };

    fn driver() -> Driver {
        let profile = Profile::from_toml("id = \"x\"\nname = \"X\"\n[signatures]\n").unwrap();
        Driver::new(profile, Policy::default(), SIZE, 0)
    }

    #[test]
    fn snapshot_is_plain_text_of_the_rendered_screen() {
        let mut driver = driver();
        driver.output(b"\x1b[1;31mAPI Error: 529\x1b[0m overloaded\r\n", 2_000);
        driver.output(b"\x1b[4;3H> \x1b[K", 3_000);
        let snap = driver.snapshot();
        // Written spaces are content, cleared cells are not.
        assert_eq!(snap.text, "API Error: 529 overloaded\n\n\n  > ");
        assert_eq!(snap.last_output_ms, 3_000);
    }

    /// vt100 0.16.2 panics when anything is printed over a wide character that a shrink cut in half.
    /// If this starts returning `Some`, vt100 fixed it upstream.
    #[test]
    fn screen_model_panics_are_contained() {
        let mut driver = driver();
        let cut_in_half = format!("{}試", "x".repeat(40));
        let fed = driver
            .output(cut_in_half.as_bytes(), 1)
            .and_then(|()| driver.resize(PtySize { cols: 41, ..SIZE }))
            .and_then(|()| driver.output(b"K", 2));
        assert!(fed.is_none());
    }
}
