//! Sessions against a fake agent in a real PTY (ConPTY on Windows), and the session loop alone. The
//! fake agent is this test binary started again with an environment variable that turns one of the
//! `fake_*` tests below from a no-op into the agent (or into `harn run` itself).

use std::ffi::{OsStr, OsString};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use fake::{
    ECHO_EXIT_CODE, ECHO_PROMPT, FAKE_AGENT_ENV, FAKE_WRAPPER_ENV, GAVE_UP_EXIT_CODE, MODE_ECHO,
    MODE_OVERLOADED, MODE_RAW, PATIENCE_ENV, RAW_EXIT_CODE, RAW_PROMPT, RESIZE_PROMPT,
    RESUMED_EXIT_CODE,
};
use harn_core::{Action, Trigger};
use harness::{RESIZED, SIZE, Step, automation, child_command, run_headless, test_policy};

use super::Session;
use crate::command;
use crate::pump::Event;
use crate::queue::InputQueue;

mod fake;
mod harness;

const DRY_RUN_PATIENCE_MS: &str = "2500";
const GAP_MS: u64 = 150;

/// Keys typed between our `continue` and its Enter would be submitted glued to it.
#[test]
fn typing_cancels_the_rest_of_an_injection() {
    let (mut session, written) = bare_session();
    let t0 = Instant::now();
    let action = Action::Input {
        chunks: vec!["continue".into(), "\r".into()],
        gap_ms: GAP_MS,
        trigger: Trigger::Overloaded,
        attempt: 1,
    };
    session.execute(action, t0);
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

/// A notification is only logged: the session has no handle on the terminal, so it cannot write a
/// bell into the middle of the agent's escape sequences, and it types nothing either.
#[test]
fn notify_only_logs() {
    let (mut session, written) = bare_session();
    let t0 = Instant::now();
    let action = Action::Notify {
        trigger: Trigger::Stalled,
        message: "no progress for 15 minutes".into(),
    };
    session.execute(action, t0);
    session.flush_due(t0 + Duration::from_millis(GAP_MS));
    assert_eq!(written.try_iter().count(), 0);
}

/// The session loop without a pseudo terminal; what it types into the agent comes out of `.1`.
fn bare_session() -> (Session, Receiver<Vec<u8>>) {
    let (pty_input, written) = mpsc::channel();
    let session = Session {
        master: None,
        size: SIZE,
        poll_size: Box::new(|| None),
        driver: None,
        dry_run: false,
        queue: InputQueue::default(),
        pty_input,
        next_tick: Instant::now(),
        output_closed: false,
    };
    (session, written)
}

#[test]
fn overloaded_agent_gets_continue_and_its_exit_code_propagates() {
    let cmd = child_command(
        &test_path("fake_agent"),
        &[(FAKE_AGENT_ENV, MODE_OVERLOADED)],
    );
    let done = run_headless(cmd, Some(automation(false)), Vec::new());
    assert_eq!(done.code, RESUMED_EXIT_CODE, "output:\n{}", done.output);
    let resumed = done.output.contains("fake agent: resumed");
    assert!(resumed, "output:\n{}", done.output);
}

#[test]
fn dry_run_never_types() {
    let env = [
        (FAKE_AGENT_ENV, MODE_OVERLOADED),
        (PATIENCE_ENV, DRY_RUN_PATIENCE_MS),
    ];
    let cmd = child_command(&test_path("fake_agent"), &env);
    let done = run_headless(cmd, Some(automation(true)), Vec::new());
    assert_eq!(done.code, GAVE_UP_EXIT_CODE, "output:\n{}", done.output);
}

#[test]
fn passthrough_forwards_keystrokes_and_output() {
    let cmd = child_command(&test_path("fake_agent"), &[(FAKE_AGENT_ENV, MODE_ECHO)]);
    let done = run_headless(cmd, None, vec![(ECHO_PROMPT, Step::Type("ping\r"))]);
    assert_eq!(done.code, ECHO_EXIT_CODE, "output:\n{}", done.output);
    let echoed = done.output.contains("echo: PING");
    assert!(echoed, "output:\n{}", done.output);
}

/// A raw-mode agent (like a real TUI) gets an arrow key, a lone Ctrl+Z and a resize.
#[test]
fn raw_mode_agent_gets_keys_and_resizes() {
    let cmd = child_command(&test_path("fake_agent"), &[(FAKE_AGENT_ENV, MODE_RAW)]);
    assert_raw_agent_finished(&run_headless(cmd, None, raw_agent_script()));
}

/// `run` itself, nested inside a PTY: real console modes, raw VT input, the terminal answering
/// ConPTY's cursor query, resize propagation and exit codes.
#[test]
fn run_drives_a_real_console_end_to_end() {
    let agent = test_path("fake_agent");
    let env = [
        (FAKE_WRAPPER_ENV, agent.as_str()),
        (FAKE_AGENT_ENV, MODE_RAW),
    ];
    let cmd = child_command(&test_path("fake_wrapper"), &env);
    assert_raw_agent_finished(&run_headless(cmd, None, raw_agent_script()));
}

fn raw_agent_script() -> Vec<(&'static str, Step)> {
    vec![
        (RESIZE_PROMPT, Step::Resize),
        (RAW_PROMPT, Step::Type("\x1b[A")),
        (r"got: \x1b[A", Step::Type("\x1a")),
        (r"got: \x1b[A\x1a", Step::Type("x\r")),
    ]
}

/// Read boundaries are not preserved in general, but an arrow key must arrive whole: a TUI would
/// read a lone ESC as the Esc key.
fn assert_raw_agent_finished(done: &harness::Finished) {
    assert_eq!(done.code, RAW_EXIT_CODE, "output:\n{}", done.output);
    let resized = format!("resized: {}x{}", RESIZED.cols, RESIZED.rows);
    for expected in [resized.as_str(), r"read: \x1b[A", r"got: \x1b[A\x1ax\r"] {
        assert!(
            done.output.contains(expected),
            "no {expected:?} in:\n{}",
            done.output
        );
    }
}

#[cfg(windows)]
#[test]
fn batch_shims_get_their_arguments_intact() {
    let dir = std::env::temp_dir().join(format!("harn pty shim {}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("echo args.cmd");
    std::fs::write(&script, "@echo off\r\necho ARGS=[%*]\r\nexit /b 7\r\n").unwrap();
    let args = ["hello world", "x(y)", "a & b", "", "what does /? do"].map(OsString::from);
    let cmd = command::build(script.as_os_str(), &args).unwrap();
    let refused = command::build(script.as_os_str(), &[OsString::from("--x=/?")]);
    let done = run_headless(cmd, None, Vec::new());
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(done.code, 7, "output:\n{}", done.output);
    let expected = r#"ARGS=["hello world" x(y) "a & b" "" "what does /? do"]"#;
    assert!(done.output.contains(expected), "output:\n{}", done.output);
    // Unquoted, `/?` makes cmd's `call` print its help and exit 0 without running the shim.
    assert!(matches!(
        refused,
        Err(crate::PtyError::BatchArgument { .. })
    ));
}

#[test]
#[ignore = "runs the locally installed claude and codex, version flags only"]
fn installed_agents_report_their_version() {
    for (agent, marker) in [("claude", "Claude Code"), ("codex", "codex-cli")] {
        let cmd = command::build(OsStr::new(agent), &[OsString::from("--version")]).unwrap();
        let done = run_headless(cmd, None, Vec::new());
        assert_eq!(done.code, 0, "{agent} output:\n{}", done.output);
        let line = done.output.lines().find(|line| line.contains(marker));
        assert!(line.is_some(), "{agent} output:\n{}", done.output);
        println!("{agent}: {}", line.unwrap_or_default().escape_debug());
    }
    let opts = crate::RunOptions {
        program: "codex".into(),
        args: vec!["--version".into()],
        profile: None,
        policy: test_policy(),
        dry_run: false,
    };
    assert_eq!(crate::run(opts).unwrap(), 0);
}

/// The fake agent; a no-op unless started by one of the tests above.
#[test]
fn fake_agent() {
    if let Some(mode) = std::env::var_os(FAKE_AGENT_ENV) {
        fake::act_as_agent(&mode);
    }
}

/// `harn run <fake agent>`; a no-op unless started by one of the tests above.
#[test]
fn fake_wrapper() {
    if let Some(agent_test) = std::env::var_os(FAKE_WRAPPER_ENV) {
        fake::act_as_wrapper(&agent_test);
    }
}

/// `name` as libtest's `--exact` filter expects it, e.g. `session::tests::fake_agent`.
fn test_path(name: &str) -> String {
    let module = module_path!().split_once("::").map_or("", |(_, rest)| rest);
    format!("{module}::{name}")
}
