//! The caller's real terminal: raw input, VT output, its size, and restoring all of it on any exit.

use std::io::{self, IsTerminal, Read};

use portable_pty::PtySize;

use crate::PtyError;
use crate::session::Terminal;

#[cfg(windows)]
mod windows;

/// Puts the terminal into raw mode (and on Windows VT input/output mode) and puts it back on drop,
/// which also runs while a panic unwinds through [`crate::run`].
#[must_use = "the terminal is restored when the guard is dropped"]
pub(crate) struct ConsoleGuard {
    #[cfg(not(windows))]
    raw: bool,
    #[cfg(windows)]
    saved: Vec<windows::SavedMode>,
}

#[cfg(not(windows))]
impl ConsoleGuard {
    pub(crate) fn enable() -> Result<Self, PtyError> {
        if !io::stdin().is_terminal() {
            tracing::debug!("stdin is not a terminal; leaving its mode alone");
            return Ok(Self { raw: false });
        }
        crossterm::terminal::enable_raw_mode().map_err(PtyError::Console)?;
        Ok(Self { raw: true })
    }
}

#[cfg(not(windows))]
impl Drop for ConsoleGuard {
    fn drop(&mut self) {
        if !self.raw {
            return;
        }
        if let Err(err) = crossterm::terminal::disable_raw_mode() {
            tracing::error!(%err, "cannot restore the terminal mode");
        }
    }
}

#[cfg(windows)]
impl ConsoleGuard {
    pub(crate) fn enable() -> Result<Self, PtyError> {
        use std::os::windows::io::AsRawHandle;

        let mut guard = Self { saved: Vec::new() };
        // Partial setup is undone by the guard's Drop when the second step fails.
        guard.adjust(io::stdin().as_raw_handle(), windows::raw_vt_input)?;
        guard.adjust(io::stdout().as_raw_handle(), windows::vt_output)?;
        Ok(guard)
    }

    fn adjust(
        &mut self,
        handle: std::os::windows::io::RawHandle,
        change: fn(u32) -> u32,
    ) -> Result<(), PtyError> {
        let Some(saved) = windows::SavedMode::read(handle) else {
            tracing::debug!("standard handle is not a console; leaving its mode alone");
            return Ok(());
        };
        saved.apply(change).map_err(PtyError::Console)?;
        self.saved.push(saved);
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for ConsoleGuard {
    fn drop(&mut self) {
        for saved in self.saved.iter().rev() {
            if let Err(err) = saved.restore() {
                tracing::error!(%err, "cannot restore the console mode");
            }
        }
    }
}

/// Size of the caller's terminal window, if there is one.
pub(crate) fn terminal_size() -> Option<PtySize> {
    // `crossterm::terminal::size` falls back to spawning `tput` on Unix; this is polled, so use the
    // ioctl-only query there.
    #[cfg(not(windows))]
    let (cols, rows) = crossterm::terminal::window_size()
        .ok()
        .map(|size| (size.columns, size.rows))?;
    #[cfg(windows)]
    let (cols, rows) = crossterm::terminal::size().ok()?;
    (cols > 0 && rows > 0).then_some(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })
}

pub(crate) fn stdio_terminal() -> Terminal {
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    Terminal {
        input: Box::new(Stdin::new()),
        output: Box::new(io::stdout()),
        size: Box::new(terminal_size),
        answer_cursor_queries: !interactive,
    }
}

/// The caller's stdin as raw bytes.
pub(crate) struct Stdin {
    inner: io::Stdin,
    #[cfg(windows)]
    console: bool,
}

impl Stdin {
    pub(crate) fn new() -> Self {
        let inner = io::stdin();
        Self {
            #[cfg(windows)]
            console: windows::is_console(&inner),
            inner,
        }
    }
}

impl Read for Stdin {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        #[cfg(windows)]
        if n == 0 && self.console && !buf.is_empty() {
            // std drops a trailing Ctrl+Z from console reads (the DOS end-of-input convention); a
            // console read returning nothing means the user pressed exactly that key.
            buf[0] = windows::CTRL_Z;
            return Ok(1);
        }
        Ok(n)
    }
}
