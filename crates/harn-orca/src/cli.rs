//! The `orca` CLI: one short-lived process per call, its JSON envelope parsed with serde.

use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use tracing::{debug, warn};

/// A hung call (Orca restarting, runtime wedged) must not stall the sidecar.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// After `orca` exits, how long a surviving child may hold its output pipes open.
const OUTPUT_GRACE: Duration = Duration::from_secs(2);
const WAIT_POLL: Duration = Duration::from_millis(10);
const STDERR_EXCERPT_CHARS: usize = 400;
const MAX_HANDLE_LEN: usize = 128;
/// No real terminal row is wider; caps the spaces restored at a soft wrap.
const MAX_ROW_WIDTH: usize = 4096;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Messages are self-contained (no `source()` chain), so logs and `{:#}` chains don't repeat them.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("could not run `{bin}`: {error}")]
    Spawn { bin: String, error: io::Error },
    #[error("`orca {command}` did not finish within {timeout:?}")]
    Timeout {
        command: &'static str,
        timeout: Duration,
    },
    #[error("i/o error while running `orca {command}`: {error}")]
    Io {
        command: &'static str,
        error: io::Error,
    },
    #[error("`orca {command}` exited with {status} without JSON output: {stderr}")]
    Exit {
        command: &'static str,
        status: ExitStatus,
        stderr: String,
    },
    #[error("`orca {command}` printed unexpected JSON: {error}")]
    Json {
        command: &'static str,
        error: serde_json::Error,
    },
    #[error("`orca {command}` failed: {code}: {message}")]
    Orca {
        command: &'static str,
        code: String,
        message: String,
    },
    #[error("orca returned {0:?} output instead of the rendered screen")]
    NotScreen(Option<String>),
    #[error("terminal {0} did not accept the input")]
    NotAccepted(String),
    #[error("refusing to address terminal handle {0:?}")]
    InvalidHandle(String),
}

/// A live terminal as reported by `orca terminal list`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TerminalInfo {
    pub handle: String,
    pub connected: bool,
    pub writable: bool,
    /// Epoch ms of the terminal's last output.
    pub last_output_at: u64,
    /// Orca's guess of the agent in the terminal; it stays after the agent exits to a shell.
    pub agent_identity: Option<String>,
}

/// A rendered screen, one logical line per `\n`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Screen {
    pub text: String,
    /// The user is composing a prompt for this terminal in Orca's own UI.
    pub has_draft: bool,
}

/// One piece of input for `orca terminal send`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Keys {
    /// Written verbatim (`--text`).
    Text(String),
    /// A bare carriage return (`--enter`).
    Enter,
}

pub(crate) trait OrcaCli {
    fn list(&mut self) -> Result<Vec<TerminalInfo>, CliError>;
    fn read_screen(&mut self, handle: &str) -> Result<Screen, CliError>;
    fn send(&mut self, handle: &str, keys: &Keys) -> Result<(), CliError>;
    /// Waits between input chunks; fakes record it instead of sleeping.
    fn pause(&mut self, ms: u64) {
        thread::sleep(Duration::from_millis(ms));
    }
}

#[derive(Deserialize)]
struct Envelope<T> {
    ok: bool,
    result: Option<T>,
    error: Option<ErrorBody>,
}

#[derive(Default, Deserialize)]
struct ErrorBody {
    code: String,
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct ListResult {
    terminals: Vec<TerminalInfo>,
}

#[derive(Deserialize)]
struct ReadResult {
    terminal: ScreenRead,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScreenRead {
    tail: Vec<String>,
    #[serde(default)]
    tail_rows: Vec<RowInfo>,
    source: Option<String>,
    draft: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct RowInfo {
    /// This row continues the previous one (soft wrap).
    wrapped: bool,
    /// Spaces trimmed from the end of this row.
    trailing_spaces: usize,
}

#[derive(Deserialize)]
struct SendResult {
    send: SendReceipt,
}

#[derive(Deserialize)]
struct SendReceipt {
    accepted: bool,
}

/// Unwraps the `{ ok, result | error }` envelope every `orca ... --json` command prints.
fn parse<T: DeserializeOwned>(command: &'static str, stdout: &[u8]) -> Result<T, CliError> {
    let envelope: Envelope<T> =
        serde_json::from_slice(stdout).map_err(|error| CliError::Json { command, error })?;
    match envelope {
        Envelope {
            ok: true,
            result: Some(result),
            ..
        } => Ok(result),
        Envelope { error, .. } => {
            let ErrorBody { code, message } = error.unwrap_or_default();
            Err(CliError::Orca {
                command,
                code,
                message,
            })
        }
    }
}

impl ScreenRead {
    /// Accumulated output stacks every repaint of a TUI, so only a rendered screen is accepted. A
    /// soft-wrapped row joins the previous one, whose trimmed trailing spaces belong to the line.
    fn into_screen(self) -> Result<Screen, CliError> {
        if self.source.as_deref() != Some("screen") {
            return Err(CliError::NotScreen(self.source));
        }
        let mut lines: Vec<String> = Vec::with_capacity(self.tail.len());
        for (i, row) in self.tail.iter().enumerate() {
            match (lines.last_mut(), self.tail_rows.get(i)) {
                (Some(line), Some(info)) if info.wrapped => {
                    let spaces = self.tail_rows[i - 1].trailing_spaces.min(MAX_ROW_WIDTH);
                    line.push_str(&" ".repeat(spaces));
                    line.push_str(row);
                }
                _ => lines.push(row.clone()),
            }
        }
        let has_draft = self.draft.is_some_and(|draft| !draft.trim().is_empty());
        Ok(Screen {
            text: lines.join("\n"),
            has_draft,
        })
    }
}

/// `--terminal=<handle>` for Orca-shaped handles only: an empty value makes orca fall back to the
/// active terminal, which may belong to someone else.
pub(crate) fn terminal_arg(handle: &str) -> Result<String, CliError> {
    let valid = handle.len() <= MAX_HANDLE_LEN
        && handle.starts_with(|c: char| c.is_ascii_alphanumeric())
        && handle
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if valid {
        Ok(format!("--terminal={handle}"))
    } else {
        Err(CliError::InvalidHandle(handle.to_owned()))
    }
}

/// Runs the real `orca` executable once per call.
pub(crate) struct ProcessCli {
    bin: PathBuf,
}

impl ProcessCli {
    pub(crate) fn new(bin: PathBuf) -> Self {
        Self { bin }
    }

    fn call<T: DeserializeOwned>(
        &self,
        command: &'static str,
        args: &[&str],
    ) -> Result<T, CliError> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Otherwise every call flashes a console window when harn itself runs without one.
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, CREATE_NO_WINDOW);
        let child = cmd.spawn().map_err(|error| CliError::Spawn {
            bin: self.bin.display().to_string(),
            error,
        })?;
        let (status, stdout, stderr) = finish(child, command, CALL_TIMEOUT)?;
        if !status.success() && stdout.trim_ascii().is_empty() {
            let stderr = String::from_utf8_lossy(&stderr);
            let stderr = stderr.trim().chars().take(STDERR_EXCERPT_CHARS).collect();
            return Err(CliError::Exit {
                command,
                status,
                stderr,
            });
        }
        parse(command, &stdout)
    }
}

impl OrcaCli for ProcessCli {
    fn list(&mut self) -> Result<Vec<TerminalInfo>, CliError> {
        let list: ListResult = self.call("terminal list", &["terminal", "list", "--json"])?;
        Ok(list.terminals)
    }

    fn read_screen(&mut self, handle: &str) -> Result<Screen, CliError> {
        let args = [
            "terminal",
            "read",
            &terminal_arg(handle)?,
            "--screen",
            "--json",
        ];
        let read: ReadResult = self.call("terminal read", &args)?;
        read.terminal.into_screen()
    }

    fn send(&mut self, handle: &str, keys: &Keys) -> Result<(), CliError> {
        // `--flag=value` keeps a value that starts with `--` from being parsed as a flag.
        let input = match keys {
            Keys::Text(text) => format!("--text={text}"),
            Keys::Enter => "--enter".to_owned(),
        };
        let args = ["terminal", "send", &terminal_arg(handle)?, &input, "--json"];
        let sent: SendResult = self.call("terminal send", &args)?;
        if sent.send.accepted {
            Ok(())
        } else {
            Err(CliError::NotAccepted(handle.to_owned()))
        }
    }
}

type PipeOutput = (bool, io::Result<Vec<u8>>);

/// Waits for `child`, killing its whole process tree after `timeout`. Output is read on detached
/// threads, so a surviving grandchild that holds the pipes cannot hang us.
fn finish(
    mut child: Child,
    command: &'static str,
    timeout: Duration,
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), CliError> {
    let (tx, rx) = mpsc::channel();
    spawn_reader(child.stdout.take(), true, tx.clone());
    spawn_reader(child.stderr.take(), false, tx);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(WAIT_POLL),
            Ok(None) => {
                kill_tree(&mut child);
                return Err(CliError::Timeout { command, timeout });
            }
            Err(error) => {
                kill_tree(&mut child);
                return Err(CliError::Io { command, error });
            }
        }
    };
    let (mut stdout, mut stderr) = (None, None);
    let output_deadline = Instant::now() + OUTPUT_GRACE;
    while stdout.is_none() || stderr.is_none() {
        let left = output_deadline.saturating_duration_since(Instant::now());
        let Ok((is_stdout, output)) = rx.recv_timeout(left) else {
            break;
        };
        let output = output.map_err(|error| CliError::Io { command, error })?;
        *(if is_stdout { &mut stdout } else { &mut stderr }) = Some(output);
    }
    let stdout = stdout.ok_or(CliError::Timeout {
        command,
        timeout: OUTPUT_GRACE,
    })?;
    Ok((status, stdout, stderr.unwrap_or_default()))
}

fn spawn_reader<R: Read + Send + 'static>(
    pipe: Option<R>,
    is_stdout: bool,
    tx: mpsc::Sender<PipeOutput>,
) {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let output = match pipe {
            Some(mut pipe) => pipe.read_to_end(&mut buf).map(|_| buf),
            None => Ok(buf),
        };
        // Once `finish` has given up nobody listens; the output is dropped then.
        let _ = tx.send((is_stdout, output));
    });
}

/// `orca` is a launcher: killing only it would leave the process it started running.
fn kill_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let killed = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status();
        if let Err(error) = killed {
            warn!(%error, "could not run taskkill; killing only orca");
        }
    }
    if let Err(error) = child.kill() {
        debug!(%error, "orca already gone");
    }
    if let Err(error) = child.try_wait() {
        debug!(%error, "could not reap orca");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(json: &str) -> Result<Screen, CliError> {
        parse::<ReadResult>("terminal read", json.as_bytes())?
            .terminal
            .into_screen()
    }

    #[test]
    fn parses_captured_list_and_error_envelopes() {
        let list: ListResult =
            parse("terminal list", include_bytes!("../tests/data/list.json")).unwrap();
        assert_eq!(list.terminals.len(), 4);
        let claude = &list.terminals[3];
        assert_eq!(claude.agent_identity.as_deref(), Some("claude"));
        assert!(claude.connected && claude.writable);
        assert_eq!(list.terminals[0].agent_identity, None);

        let stale = r#"{"ok":false,"error":{"code":"terminal_handle_stale","message":"x"}}"#;
        let err = parse::<ListResult>("terminal list", stale.as_bytes()).unwrap_err();
        assert!(matches!(err, CliError::Orca { code, .. } if code == "terminal_handle_stale"));
        let malformed = r#"{"ok":true,"result":{"terminals":[{"handle":"t"}]}}"#;
        let err = parse::<ListResult>("terminal list", malformed.as_bytes()).unwrap_err();
        assert!(matches!(err, CliError::Json { .. }));
    }

    #[test]
    fn screen_joins_wrapped_rows_and_rejects_accumulated_output() {
        let wrapped = r#"{"ok":true,"result":{"terminal":{"source":"screen",
            "tail":["API Error: 529 overloaded","_error","❯"],
            "tailRows":[{"trailingSpaces":1},{"wrapped":true},{}]}}}"#;
        let text = screen(wrapped).unwrap().text;
        assert_eq!(text, "API Error: 529 overloaded _error\n❯");
        let stream = r#"{"ok":true,"result":{"terminal":{"source":"stream","tail":["x"]}}}"#;
        assert!(matches!(screen(stream), Err(CliError::NotScreen(_))));
        let agent = include_str!("../tests/data/read_screen_agent.json");
        assert!(screen(agent).unwrap().text.contains("● API Error: 529"));
    }

    #[test]
    fn orca_composer_draft_is_reported() {
        let read = |draft: &str| {
            let json = format!(
                r#"{{"ok":true,"result":{{"terminal":{{"source":"screen","tail":["❯"],"draft":{draft}}}}}}}"#
            );
            screen(&json).unwrap().has_draft
        };
        assert!(read(r#""fix the build""#));
        assert!(!read(r#""  ""#) && !read("null"));
    }

    #[test]
    fn only_orca_shaped_handles_reach_the_cli() {
        let mut cli = ProcessCli::new(PathBuf::from("orca-binary-that-does-not-exist"));
        for handle in ["", " ", "--enter", "term a", "term_a\n", &"t".repeat(200)] {
            assert!(matches!(
                cli.read_screen(handle),
                Err(CliError::InvalidHandle(_))
            ));
            assert!(matches!(
                cli.send(handle, &Keys::Enter),
                Err(CliError::InvalidHandle(_))
            ));
        }
        assert!(terminal_arg("term_16579e3b-19de-4620-8e63-89b8676e3403").is_ok());
    }

    /// `cmd` stands in for the orca launcher; killing only it would leave `ping` holding the pipe.
    #[cfg(windows)]
    #[test]
    fn timeout_kills_the_whole_process_tree() {
        let launcher = || {
            Command::new("cmd")
                .args(["/C", "ping -n 30 127.0.0.1"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        };
        let started = Instant::now();
        let err = finish(launcher(), "terminal list", Duration::from_millis(300)).unwrap_err();
        assert!(matches!(err, CliError::Timeout { .. }), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(5));

        let mut child = launcher();
        let (tx, rx) = mpsc::channel();
        spawn_reader(child.stdout.take(), true, tx);
        thread::sleep(Duration::from_millis(300));
        kill_tree(&mut child);
        let eof = rx.recv_timeout(Duration::from_secs(3));
        assert!(eof.is_ok(), "ping survived and still holds the pipe");
    }
}
