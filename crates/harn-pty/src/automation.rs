//! The agent's rendered screen and the engine that watches it.

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};

use harn_core::{Action, Engine, Policy, Profile, Snapshot};
use portable_pty::PtySize;

/// No scrollback: the engine only looks at what is on screen.
const SCROLLBACK_ROWS: usize = 0;

/// What `run` was asked to automate.
#[derive(Debug)]
pub(crate) struct Automation {
    pub(crate) profile: Profile,
    pub(crate) policy: Policy,
    pub(crate) dry_run: bool,
}

/// The engine or its screen model panicked (already logged); automation is over for this session.
#[derive(Debug)]
pub(crate) struct EngineFailed;

/// The agent's terminal as the user sees it, rebuilt from the raw output.
pub(crate) struct Screen {
    parser: vt100::Parser,
    last_output_ms: u64,
}

impl Screen {
    pub(crate) fn new(size: PtySize, now_ms: u64) -> Self {
        Self {
            parser: vt100::Parser::new(size.rows, size.cols, SCROLLBACK_ROWS),
            last_output_ms: now_ms,
        }
    }

    pub(crate) fn output(&mut self, bytes: &[u8], now_ms: u64) {
        self.parser.process(bytes);
        self.last_output_ms = now_ms;
    }

    pub(crate) fn resize(&mut self, size: PtySize) {
        self.parser.screen_mut().set_size(size.rows, size.cols);
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.parser.screen().contents(),
            last_output_ms: self.last_output_ms,
        }
    }
}

pub(crate) struct Driver {
    screen: Screen,
    engine: Engine,
}

impl Driver {
    pub(crate) fn new(profile: Profile, policy: Policy, screen: Screen) -> Self {
        Self {
            screen,
            engine: Engine::new(profile, policy),
        }
    }

    pub(crate) fn output(&mut self, bytes: &[u8], now_ms: u64) -> Result<(), EngineFailed> {
        guarded("screen output", || self.screen.output(bytes, now_ms))
    }

    pub(crate) fn resize(&mut self, size: PtySize) -> Result<(), EngineFailed> {
        guarded("screen resize", || self.screen.resize(size))
    }

    pub(crate) fn user_input(&mut self, now_ms: u64) -> Result<(), EngineFailed> {
        guarded("note_user_input", || self.engine.note_user_input(now_ms))
    }

    pub(crate) fn step(&mut self, now_ms: u64) -> Result<Vec<Action>, EngineFailed> {
        guarded("step", || {
            let snapshot = self.screen.snapshot();
            self.engine.step(&snapshot, now_ms)
        })
    }
}

/// A bug in the engine or in vt100 (which panics on some wide-character/resize combinations) must
/// not take the agent down with it: the passthrough keeps running and only the automation stops.
/// The driver is never used again after a panic, so whatever state it was left in does not matter.
fn guarded<T>(call: &str, f: impl FnOnce() -> T) -> Result<T, EngineFailed> {
    panic::catch_unwind(AssertUnwindSafe(f)).map_err(|payload| {
        tracing::error!(
            call,
            panic = panic_message(payload.as_ref()),
            "auto-continue engine panicked; automation is off for the rest of this session"
        );
        EngineFailed
    })
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: PtySize = PtySize {
        rows: 6,
        cols: 40,
        pixel_width: 0,
        pixel_height: 0,
    };

    #[test]
    fn snapshot_is_plain_text_of_the_rendered_screen() {
        let mut screen = Screen::new(SIZE, 1_000);
        screen.output(b"\x1b[1;31mAPI Error: 529\x1b[0m overloaded\r\n", 2_000);
        screen.output(b"\x1b[4;3H> \x1b[7m \x1b[0m\x1b[K", 3_000);
        let snap = screen.snapshot();
        // Written spaces are content (the cursor cell here), cleared cells are not.
        assert_eq!(snap.text, "API Error: 529 overloaded\n\n\n  >  ");
        assert_eq!(snap.last_output_ms, 3_000);
    }

    #[test]
    fn redraws_replace_old_content() {
        let mut screen = Screen::new(SIZE, 0);
        screen.output(b"Thinking... (esc to interrupt)", 1);
        screen.output(b"\r\x1b[2KDone.", 2);
        assert_eq!(screen.snapshot().text, "Done.");
    }

    #[test]
    fn resize_changes_the_wrap_width() {
        let mut screen = Screen::new(SIZE, 0);
        screen.resize(PtySize { cols: 10, ..SIZE });
        screen.output(b"0123456789abc\r\n", 1);
        assert_eq!(screen.parser.screen().size(), (SIZE.rows, 10));
        assert_eq!(screen.parser.screen().cursor_position(), (2, 0));
        assert_eq!(screen.snapshot().text, "0123456789abc");
    }

    /// vt100 0.16.2 panics when anything is printed over a wide character that a shrink cut in half
    /// (fuzzing hit it at up to 60 columns). If this starts returning `Ok`, vt100 fixed it upstream.
    #[test]
    fn screen_model_panics_are_contained() {
        let profile = Profile::from_toml("id = \"x\"\nname = \"X\"\n[signatures]\n").unwrap();
        let wide = Screen::new(PtySize { cols: 120, ..SIZE }, 0);
        let mut driver = Driver::new(profile, Policy::default(), wide);
        let cut_in_half = format!("{}試", "x".repeat(40));
        let fed = driver
            .output(cut_in_half.as_bytes(), 1)
            .and_then(|()| driver.resize(PtySize { cols: 41, ..SIZE }))
            .and_then(|()| driver.output(b"K", 2));
        assert!(fed.is_err());
    }

    #[test]
    fn panics_are_contained() {
        assert!(guarded("ok", || 5).is_ok_and(|v| v == 5));
        assert!(guarded("boom", || -> u8 { panic!("engine bug") }).is_err());
    }

    #[test]
    fn panic_messages_are_extracted() {
        let static_str: Box<dyn Any + Send> = Box::new("static");
        let owned: Box<dyn Any + Send> = Box::new(String::from("owned"));
        let other: Box<dyn Any + Send> = Box::new(7_u8);
        assert_eq!(panic_message(static_str.as_ref()), "static");
        assert_eq!(panic_message(owned.as_ref()), "owned");
        assert_eq!(panic_message(other.as_ref()), "non-string panic payload");
    }
}
