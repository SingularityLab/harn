//! Console modes, the crate's only FFI.

use std::io;
use std::os::windows::io::{AsRawHandle, RawHandle};

use windows_sys::Win32::System::Console::{
    CONSOLE_MODE, DISABLE_NEWLINE_AUTO_RETURN, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
    ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, SetConsoleMode,
};

pub(super) const CTRL_Z: u8 = 0x1a;

/// A console handle together with the mode it had before we touched it.
pub(super) struct SavedMode {
    handle: RawHandle,
    mode: CONSOLE_MODE,
}

impl SavedMode {
    /// `None` when the handle is not a console (redirected, or no console at all).
    pub(super) fn read(handle: RawHandle) -> Option<Self> {
        get_mode(handle).map(|mode| Self { handle, mode })
    }

    pub(super) fn apply(&self, change: fn(CONSOLE_MODE) -> CONSOLE_MODE) -> io::Result<()> {
        set_mode(self.handle, change(self.mode))
    }

    pub(super) fn restore(&self) -> io::Result<()> {
        set_mode(self.handle, self.mode)
    }
}

pub(super) fn is_console(stream: &impl AsRawHandle) -> bool {
    get_mode(stream.as_raw_handle()).is_some()
}

/// Keys arrive as VT sequences (arrows, mouse, bracketed paste) and Ctrl+C is a byte for the agent,
/// not a signal for us.
pub(super) fn raw_vt_input(mode: CONSOLE_MODE) -> CONSOLE_MODE {
    (mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
        | ENABLE_VIRTUAL_TERMINAL_INPUT
}

/// The pseudo console renders for an xterm-like terminal, where LF does not imply CR.
pub(super) fn vt_output(mode: CONSOLE_MODE) -> CONSOLE_MODE {
    mode | ENABLE_PROCESSED_OUTPUT
        | ENABLE_VIRTUAL_TERMINAL_PROCESSING
        | DISABLE_NEWLINE_AUTO_RETURN
}

#[allow(unsafe_code)]
fn get_mode(handle: RawHandle) -> Option<CONSOLE_MODE> {
    let mut mode: CONSOLE_MODE = 0;
    // SAFETY: GetConsoleMode only writes through `mode`, a live local. A null, invalid or
    // non-console handle makes the call fail, which is reported as `None`.
    let ok = unsafe { GetConsoleMode(handle, &mut mode) };
    (ok != 0).then_some(mode)
}

#[allow(unsafe_code)]
fn set_mode(handle: RawHandle, mode: CONSOLE_MODE) -> io::Result<()> {
    // SAFETY: SetConsoleMode takes the handle and mode by value and touches no memory of ours; a bad
    // handle or mode makes it fail, which is reported as an error.
    let ok = unsafe { SetConsoleMode(handle, mode) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
