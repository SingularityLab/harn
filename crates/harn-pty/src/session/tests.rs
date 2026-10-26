//! Sessions against a fake agent in a real PTY (ConPTY on Windows), and the session loop alone. The
//! fake agent is this test binary started again with an environment variable that turns one of the
//! `fake_*` tests below from a no-op into the agent (or into `harn run` itself).

use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use harn_core::{Action, Policy, Trigger};
use harness::{RESIZED, SIZE, Step, automation, child_command, fake_args, run_headless};

use super::Session;
use crate::automation::Automation;
use crate::console::{self, ConsoleGuard};
use crate::pump::Event;
use crate::queue::InputQueue;

mod harness;

const FAKE_AGENT_ENV: &str = "HARN_FAKE_AGENT";
/// Value: the libtest path of the agent to wrap.
const FAKE_WRAPPER_ENV: &str = "HARN_FAKE_WRAPPER";
const MODE_RAW: &str = "raw";
const RESUMED_EXIT_CODE: i32 = 42;
const RAW_EXIT_CODE: i32 = 9;
const GAVE_UP_EXIT_CODE: i32 = 3;
const RAW_PROMPT: &str = "RAW>";
const RESIZE_PROMPT: &str = "waiting for resize";
const PATIENCE: Duration = Duration::from_secs(30);
const GAP_MS: u64 = 150;
const OVERLOAD_ERROR: &str =
    r#"API Error: 529 {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;

/// A session without a pseudo terminal; `written` yields what it would type into the agent.
fn idle_session(
    dry_run: bool,
    actions: Option<mpsc::Sender<Action>>,
) -> (Session, mpsc::Receiver<Vec<u8>>) {
    let (pty_input, written) = mpsc::channel();
    let session = Session {
        master: None,
        size: SIZE,
        poll_size: Box::new(|| None),
        driver: None,
        dry_run,
        queue: InputQueue::default(),
        pty_input,
        actions,
        next_tick: Instant::now(),
        output_closed: false,
    };
    (session, written)
}

fn continue_input() -> Action {
    Action::Input {
        chunks: vec!["continue".into(), "\r".into()],
        gap_ms: GAP_MS,
        trigger: Trigger::Overloaded,
        attempt: 1,
    }
}

fn call_for_help() -> Action {
    Action::Notify {
        trigger: Trigger::UsageLimit,
        message: "gave up".into(),
    }
}

/// Keys typed between our `continue` and its Enter would be submitted glued to it.
#[test]
fn typing_cancels_the_rest_of_an_injection() {
    let (mut session, written) = idle_session(false, None);
    let t0 = Instant::now();
    session.execute(continue_input(), t0);
    session.flush_due(t0);
    assert_eq!(written.try_iter().collect::<Vec<_>>(), [b"continue"]);
    session.handle(Event::UserInput);
    session.flush_due(t0 + Duration::from_millis(GAP_MS));
    assert_eq!(
        written.try_iter().count(),
        0,
        "Enter followed the user's keys"
    );
}

#[test]
fn executed_actions_are_sent_to_the_caller() {
    let (actions, reported) = mpsc::channel();
    let (mut session, _written) = idle_session(false, Some(actions));
    let t0 = Instant::now();
    session.execute(continue_input(), t0);
    session.execute(call_for_help(), t0);
    let reported: Vec<_> = reported.try_iter().collect();
    assert_eq!(reported, [continue_input(), call_for_help()]);
}

#[test]
fn a_dry_run_reports_calls_for_help_but_not_typing() {
    let (actions, reported) = mpsc::channel();
    let (mut session, written) = idle_session(true, Some(actions));
    let t0 = Instant::now();
    session.execute(continue_input(), t0);
    session.execute(call_for_help(), t0);
    session.flush_due(t0);
    assert_eq!(written.try_iter().count(), 0, "a dry run typed");
    assert_eq!(reported.try_iter().collect::<Vec<_>>(), [call_for_help()]);
}

#[test]
fn a_gone_receiver_does_not_stop_the_typing() {
    let (actions, reported) = mpsc::channel();
    drop(reported);
    let (mut session, written) = idle_session(false, Some(actions));
    let t0 = Instant::now();
    session.execute(continue_input(), t0);
    session.flush_due(t0);
    assert_eq!(written.try_iter().collect::<Vec<_>>(), [b"continue"]);
    assert!(
        session.actions.is_none(),
        "kept sending to a closed channel"
    );
}

#[test]
fn overloaded_agent_gets_continue_and_its_exit_code_propagates() {
    let (actions, reported) = mpsc::channel();
    let automation = Automation {
        actions: Some(actions),
        ..automation()
    };
    let cmd = child_command(&test_path("fake_agent"), &[(FAKE_AGENT_ENV, "overloaded")]);
    let done = run_headless(cmd, Some(automation), Vec::new());
    assert_eq!(done.code, RESUMED_EXIT_CODE, "output:\n{}", done.output);
    let resumed = done.output.contains("fake agent: resumed");
    assert!(resumed, "output:\n{}", done.output);
    let nudged = |action: Action| {
        matches!(
            action,
            Action::Input {
                trigger: Trigger::Overloaded,
                attempt: 1,
                ..
            }
        )
    };
    assert!(
        reported.try_iter().any(nudged),
        "the nudge was not reported"
    );
}

/// `run` itself, nested inside a PTY: real console modes (restored on drop and on unwind), raw VT
/// input, the terminal answering ConPTY's cursor query, resize propagation and exit codes.
#[test]
fn run_drives_a_real_console_end_to_end() {
    let agent = test_path("fake_agent");
    let env = [
        (FAKE_WRAPPER_ENV, agent.as_str()),
        (FAKE_AGENT_ENV, MODE_RAW),
    ];
    let script = vec![
        (RESIZE_PROMPT, Step::Resize),
        (RAW_PROMPT, Step::Type("\x1b[A")),
        (r"got: \x1b[A", Step::Type("\x1a")),
        (r"got: \x1b[A\x1a", Step::Type("x\r")),
    ];
    let done = run_headless(
        child_command(&test_path("fake_wrapper"), &env),
        None,
        script,
    );
    assert_eq!(done.code, RAW_EXIT_CODE, "output:\n{}", done.output);
    let resized = format!("resized: {}x{}", RESIZED.cols, RESIZED.rows);
    // Read boundaries are not preserved in general, but a TUI reads a lone ESC as the Esc key.
    let expected = [
        resized.as_str(),
        r"read: \x1b[A",
        r"got: \x1b[A\x1ax\r",
        "restored: true",
    ];
    for expected in expected {
        let found = done.output.contains(expected);
        assert!(found, "no {expected:?} in:\n{}", done.output);
    }
}

#[cfg(windows)]
#[test]
fn batch_shims_get_their_arguments_intact() {
    use std::ffi::OsString;

    use crate::command;

    let dir = std::env::temp_dir().join(format!("harn pty shim {}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("echo args.cmd");
    std::fs::write(&script, "@echo off\r\necho ARGS=[%*]\r\nexit /b 7\r\n").unwrap();
    let args = ["hello world", "x(y)", "a & b", "", "what does /? do"].map(OsString::from);
    let cmd = command::build(script.as_os_str(), &args).unwrap();
    let done = run_headless(cmd, None, Vec::new());
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(done.code, 7, "output:\n{}", done.output);
    let expected = r#"ARGS=["hello world" x(y) "a & b" "" "what does /? do"]"#;
    assert!(done.output.contains(expected), "output:\n{}", done.output);
}

/// The fake agent; a no-op unless started by one of the tests above.
#[test]
fn fake_agent() {
    let Some(mode) = std::env::var_os(FAKE_AGENT_ENV) else {
        return;
    };
    std::process::exit(if mode == MODE_RAW {
        raw_agent()
    } else {
        overloaded_agent()
    });
}

/// `harn run <fake agent>`; a no-op unless started by one of the tests above.
#[test]
fn fake_wrapper() {
    let Some(agent_test) = std::env::var_os(FAKE_WRAPPER_ENV) else {
        return;
    };
    let opts = crate::RunOptions {
        program: std::env::current_exe().unwrap().into(),
        args: fake_args(&agent_test.to_string_lossy()),
        profile: None,
        policy: Policy::default(),
        dry_run: false,
        actions: None,
    };
    std::process::exit(crate::run(opts).unwrap());
}

/// Claude's final overload error above its empty input row; resumes once "continue" is entered.
fn overloaded_agent() -> i32 {
    let rule = "─".repeat(44);
    print!("● Fixing the flaky test\n● {OVERLOAD_ERROR}\n\n{rule}\n❯ \n{rule}\n");
    io::stdout().flush().unwrap();
    let mut typed = Vec::new();
    // Cooked mode: a read returns only once Enter was pressed.
    let resumed = read_until(|chunk| {
        typed.extend_from_slice(chunk);
        typed.windows(b"continue".len()).any(|w| w == b"continue")
    });
    if !resumed {
        return GAVE_UP_EXIT_CODE;
    }
    println!("fake agent: resumed");
    RESUMED_EXIT_CODE
}

/// A TUI in raw VT mode: waits to be resized, then reports each read and all input so far until
/// Enter, and finally whether its console modes came back.
fn raw_agent() -> i32 {
    let console = ConsoleGuard::enable().unwrap();
    // Raw output: no LF → CRLF translation.
    print!("{RESIZE_PROMPT}\r\n");
    io::stdout().flush().unwrap();
    let deadline = Instant::now() + PATIENCE;
    let resized = |s: portable_pty::PtySize| (s.cols, s.rows) == (RESIZED.cols, RESIZED.rows);
    while !console::terminal_size().is_some_and(resized) {
        if Instant::now() > deadline {
            return GAVE_UP_EXIT_CODE;
        }
        thread::sleep(Duration::from_millis(20));
    }
    print!("resized: {}x{}\r\n{RAW_PROMPT}", RESIZED.cols, RESIZED.rows);
    io::stdout().flush().unwrap();
    let mut received = Vec::new();
    let entered = read_until(|chunk| {
        received.extend_from_slice(chunk);
        print!(
            "read: {}\r\ngot: {}\r\n",
            chunk.escape_ascii(),
            received.escape_ascii()
        );
        io::stdout().flush().unwrap();
        received.contains(&b'\r')
    });
    drop(console);
    let unwound = std::panic::catch_unwind(|| {
        let _console = ConsoleGuard::enable();
        panic!("unwinding through the console guard");
    });
    let raw = crossterm::terminal::is_raw_mode_enabled().unwrap();
    print!("restored: {}\r\n", unwound.is_err() && !raw);
    if entered {
        RAW_EXIT_CODE
    } else {
        GAVE_UP_EXIT_CODE
    }
}

/// Feeds each read from stdin to `done` until it returns true; false after [`PATIENCE`].
fn read_until(mut done: impl FnMut(&[u8]) -> bool) -> bool {
    let (tx, reads) = mpsc::channel();
    thread::spawn(move || {
        let (mut stdin, mut buf) = (console::Stdin::new(), [0; 1024]);
        while let Ok(n @ 1..) = stdin.read(&mut buf) {
            if tx.send(buf[..n].to_vec()).is_err() {
                return;
            }
        }
    });
    let deadline = Instant::now() + PATIENCE;
    while let Ok(chunk) = reads.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        if done(&chunk) {
            return true;
        }
    }
    false
}

/// `name` as libtest's `--exact` filter expects it, e.g. `session::tests::fake_agent`.
fn test_path(name: &str) -> String {
    let module = module_path!().split_once("::").map_or("", |(_, rest)| rest);
    format!("{module}::{name}")
}
