//! The child side: this test binary acting as an agent, or as `harn run` wrapping one.

use std::ffi::OsStr;
use std::io::{self, BufRead, Read, Write};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use harn_core::Policy;

use super::harness::{RESIZED, fake_args};
use crate::console::{self, ConsoleGuard};

pub(super) const FAKE_AGENT_ENV: &str = "HARN_FAKE_AGENT";
/// Value: the libtest path of the agent to wrap.
pub(super) const FAKE_WRAPPER_ENV: &str = "HARN_FAKE_WRAPPER";
pub(super) const PATIENCE_ENV: &str = "HARN_FAKE_AGENT_PATIENCE_MS";
pub(super) const MODE_OVERLOADED: &str = "overloaded";
pub(super) const MODE_ECHO: &str = "echo";
pub(super) const MODE_RAW: &str = "raw";
pub(super) const RESUMED_EXIT_CODE: i32 = 42;
pub(super) const ECHO_EXIT_CODE: i32 = 7;
pub(super) const RAW_EXIT_CODE: i32 = 9;
pub(super) const GAVE_UP_EXIT_CODE: i32 = 3;
const WRAPPER_FAILED_EXIT_CODE: i32 = 4;
pub(super) const ECHO_PROMPT: &str = "READY>";
pub(super) const RAW_PROMPT: &str = "RAW>";
pub(super) const RESIZE_PROMPT: &str = "waiting for resize";
const DEFAULT_PATIENCE: Duration = Duration::from_secs(30);
const SIZE_POLL: Duration = Duration::from_millis(20);
const READ_BUF_BYTES: usize = 1024;
const OVERLOAD_ERROR: &str =
    r#"API Error: 529 {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;

pub(super) fn act_as_agent(mode: &OsStr) -> ! {
    let patience = std::env::var(PATIENCE_ENV)
        .ok()
        .and_then(|ms| ms.parse().ok())
        .map_or(DEFAULT_PATIENCE, Duration::from_millis);
    let code = match mode.to_str() {
        Some(MODE_OVERLOADED) => overloaded(patience),
        Some(MODE_ECHO) => echo(patience),
        Some(MODE_RAW) => raw(patience),
        _ => panic!("unknown fake agent mode {mode:?}"),
    };
    std::process::exit(code);
}

pub(super) fn act_as_wrapper(agent_test: &OsStr) -> ! {
    let opts = crate::RunOptions {
        program: std::env::current_exe().unwrap().into(),
        args: fake_args(&agent_test.to_string_lossy()),
        profile: None,
        policy: Policy::default(),
        dry_run: false,
    };
    let code = crate::run(opts).unwrap_or_else(|err| {
        eprintln!("fake wrapper: {err}");
        WRAPPER_FAILED_EXIT_CODE
    });
    std::process::exit(code);
}

/// Claude's final overload error above its input box; resumes once a line with "continue" comes.
fn overloaded(patience: Duration) -> i32 {
    println!("● Fixing the flaky test");
    println!("● {OVERLOAD_ERROR}");
    println!();
    println!("────────────────────────────────────────────");
    println!("❯ ");
    println!("────────────────────────────────────────────");
    flush();
    let lines = lines();
    if wait_for(&lines, patience, |line: &String| line.contains("continue")).is_none() {
        return GAVE_UP_EXIT_CODE;
    }
    println!("fake agent: resumed");
    flush();
    RESUMED_EXIT_CODE
}

/// Cooked mode: one line in, upper-cased line out.
fn echo(patience: Duration) -> i32 {
    print!("{ECHO_PROMPT}");
    flush();
    let Some(line) = wait_for(&lines(), patience, |_| true) else {
        return GAVE_UP_EXIT_CODE;
    };
    println!("echo: {}", line.trim().to_uppercase());
    flush();
    ECHO_EXIT_CODE
}

/// Raw VT mode like a real TUI: waits to be resized, then reports each read and everything
/// received so far (escaped) until Enter.
fn raw(patience: Duration) -> i32 {
    let _console = ConsoleGuard::enable().unwrap();
    // Raw output: no LF → CRLF translation.
    print!("{RESIZE_PROMPT}\r\n");
    flush();
    if !wait_for_resize(patience) {
        return GAVE_UP_EXIT_CODE;
    }
    print!("resized: {}x{}\r\n{RAW_PROMPT}", RESIZED.cols, RESIZED.rows);
    flush();
    let reads = raw_reads();
    let mut received = Vec::new();
    loop {
        let Some(chunk) = wait_for(&reads, patience, |_| true) else {
            return GAVE_UP_EXIT_CODE;
        };
        received.extend_from_slice(&chunk);
        print!(
            "read: {}\r\ngot: {}\r\n",
            chunk.escape_ascii(),
            received.escape_ascii()
        );
        flush();
        if received.contains(&b'\r') {
            return RAW_EXIT_CODE;
        }
    }
}

fn wait_for_resize(patience: Duration) -> bool {
    let deadline = Instant::now() + patience;
    while Instant::now() < deadline {
        let size = console::terminal_size();
        if size.is_some_and(|s| (s.cols, s.rows) == (RESIZED.cols, RESIZED.rows)) {
            return true;
        }
        thread::sleep(SIZE_POLL);
    }
    false
}

fn flush() {
    io::stdout().flush().unwrap();
}

fn lines() -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    rx
}

fn raw_reads() -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut stdin = console::Stdin::new();
        let mut buf = [0; READ_BUF_BYTES];
        while let Ok(n @ 1..) = stdin.read(&mut buf) {
            if tx.send(buf[..n].to_vec()).is_err() {
                return;
            }
        }
    });
    rx
}

fn wait_for<T>(items: &Receiver<T>, patience: Duration, wanted: impl Fn(&T) -> bool) -> Option<T> {
    let deadline = Instant::now() + patience;
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        let item = items.recv_timeout(left).ok()?;
        if wanted(&item) {
            return Some(item);
        }
    }
}
