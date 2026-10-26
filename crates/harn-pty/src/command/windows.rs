//! Windows program lookup and the rules for handing arguments to a batch file through cmd.exe.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::PtyError;

const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";
/// What CreateProcess can start: PE images directly, batch files through cmd.exe. Other PATHEXT
/// entries (.JS, .PS1, ...) need a shell association, and npm's extensionless `claude` next to
/// `claude.cmd` is a POSIX shell script.
const LAUNCHABLE: [&str; 4] = ["com", "exe", "bat", "cmd"];
const BATCH: [&str; 2] = ["bat", "cmd"];
/// cmd.exe changes these even inside quotes (`call` also doubles carets), or they break quoting.
const CMD_ALWAYS_SPECIAL: [char; 5] = ['"', '%', '^', '\r', '\n'];
/// Special to cmd.exe outside quotes.
const CMD_UNQUOTED_SPECIAL: [char; 4] = ['&', '|', '<', '>'];
/// Anywhere in an unquoted argument it makes `call` print its help instead of running the shim.
const CMD_HELP_SWITCH: &str = "/?";
/// portable-pty quotes an argument only if it is empty or contains one of these.
const QUOTE_TRIGGERS: [char; 4] = [' ', '\t', '\n', '\u{b}'];

/// Finds `program` the way a shell would: a path with a directory part is taken relative to the
/// current directory, a bare name is searched in `path` (first directory wins, PATHEXT order within
/// it). A name without a launchable extension gets one appended.
pub(super) fn resolve(
    program: &OsStr,
    path: Option<&OsStr>,
    pathext: Option<&OsStr>,
) -> Option<PathBuf> {
    let exts = launchable_extensions(pathext);
    let program = Path::new(program);
    if super::has_dir(program) || program.is_absolute() {
        return find_file(program, &exts);
    }
    search_dirs(path?).find_map(|dir| find_file(&dir.join(program), &exts))
}

/// An empty entry (`a;;b`, a trailing `;`) joins to the bare name, i.e. the current directory, where
/// a planted `claude.cmd` in an untrusted checkout would win. std's `Command` skips them too.
fn search_dirs(path: &OsStr) -> impl Iterator<Item = PathBuf> + '_ {
    std::env::split_paths(path).filter(|dir| !dir.as_os_str().is_empty())
}

pub(super) fn is_batch(path: &Path) -> bool {
    has_extension(path, &BATCH)
}

/// cmd.exe parses a batch file's command line again, so not every argument survives the trip.
/// Refuse those that would not, instead of handing the agent something else.
pub(super) fn check_batch_args(script: &Path, args: &[OsString]) -> Result<(), PtyError> {
    let script_os = script.as_os_str();
    let rejected = std::iter::once(script_os)
        .chain(args.iter().map(OsString::as_os_str))
        .find(|arg| !survives_cmd(arg));
    match rejected {
        None => Ok(()),
        Some(arg) => Err(PtyError::BatchArgument {
            script: script.display().to_string(),
            arg: arg.to_string_lossy().into_owned(),
        }),
    }
}

pub(super) fn command_interpreter() -> OsString {
    std::env::var_os("ComSpec")
        .filter(|path| Path::new(path).is_absolute())
        .unwrap_or_else(|| "cmd.exe".into())
}

fn survives_cmd(arg: &OsStr) -> bool {
    let arg = arg.to_string_lossy();
    if arg.contains(&CMD_ALWAYS_SPECIAL[..]) {
        return false;
    }
    let quoted = arg.is_empty() || arg.contains(&QUOTE_TRIGGERS[..]);
    quoted || !(arg.contains(&CMD_UNQUOTED_SPECIAL[..]) || arg.contains(CMD_HELP_SWITCH))
}

fn launchable_extensions(pathext: Option<&OsStr>) -> Vec<String> {
    let raw = pathext.and_then(OsStr::to_str).unwrap_or(DEFAULT_PATHEXT);
    let exts: Vec<String> = raw
        .split(';')
        .map(|ext| ext.trim().trim_start_matches('.'))
        .filter(|ext| LAUNCHABLE.iter().any(|ok| ok.eq_ignore_ascii_case(ext)))
        .map(|ext| format!(".{}", ext.to_ascii_lowercase()))
        .collect();
    if exts.is_empty() {
        return launchable_extensions(Some(OsStr::new(DEFAULT_PATHEXT)));
    }
    exts
}

fn find_file(base: &Path, exts: &[String]) -> Option<PathBuf> {
    if has_extension(base, &LAUNCHABLE) {
        return base.is_file().then(|| base.to_path_buf());
    }
    exts.iter()
        .map(|ext| {
            let mut candidate = base.as_os_str().to_os_string();
            candidate.push(ext);
            PathBuf::from(candidate)
        })
        .find(|candidate| candidate.is_file())
}

fn has_extension(path: &Path, wanted: &[&str]) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| wanted.iter().any(|w| w.eq_ignore_ascii_case(ext)))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("harn-pty {name} {}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn touch(&self, file: &str) -> PathBuf {
            let path = self.0.join(file);
            fs::write(&path, b"").unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn lookup(program: &str, dirs: &[&TempDir], pathext: Option<&str>) -> Option<PathBuf> {
        let path = std::env::join_paths(dirs.iter().map(|d| &d.0)).unwrap();
        resolve(OsStr::new(program), Some(&path), pathext.map(OsStr::new))
    }

    #[test]
    fn prefers_the_cmd_shim_over_npms_shell_script() {
        let npm = TempDir::new("npm");
        npm.touch("claude");
        npm.touch("claude.ps1");
        let shim = npm.touch("claude.cmd");
        assert_eq!(lookup("claude", &[&npm], None), Some(shim));
    }

    #[test]
    fn first_directory_wins_then_pathext_order() {
        let first = TempDir::new("first");
        let second = TempDir::new("second");
        let shim = first.touch("tool.cmd");
        second.touch("tool.exe");
        assert_eq!(lookup("tool", &[&first, &second], None), Some(shim));
        let exe = first.touch("tool.exe");
        assert_eq!(lookup("tool", &[&first, &second], None), Some(exe));
    }

    #[test]
    fn skips_extensions_createprocess_cannot_start() {
        let dir = TempDir::new("pathext");
        dir.touch("agent.js");
        let shim = dir.touch("agent.cmd");
        assert_eq!(lookup("agent", &[&dir], Some(".JS;.CMD")), Some(shim));
        assert_eq!(
            lookup("agent", &[&dir], Some(".JS")),
            Some(dir.0.join("agent.cmd"))
        );
    }

    #[test]
    fn explicit_names_and_paths() {
        let dir = TempDir::new("explicit");
        let shim = dir.touch("agent.cmd");
        let dotted = dir.touch("my.agent.exe");
        assert_eq!(lookup("agent.cmd", &[&dir], None), Some(shim.clone()));
        assert_eq!(lookup("my.agent", &[&dir], None), Some(dotted));
        let by_path = resolve(dir.0.join("agent").as_os_str(), None, None);
        assert_eq!(by_path, Some(shim));
        assert_eq!(lookup("missing", &[&dir], None), None);
        assert_eq!(resolve(OsStr::new("agent"), None, None), None);
    }

    #[test]
    fn empty_path_entries_are_skipped() {
        let dirs: Vec<_> = search_dirs(OsStr::new(r";C:\a;;C:\b;")).collect();
        assert_eq!(dirs, [PathBuf::from(r"C:\a"), PathBuf::from(r"C:\b")]);
    }

    #[test]
    fn batch_detection_ignores_case() {
        assert!(is_batch(Path::new(r"C:\npm\claude.CMD")));
        assert!(is_batch(Path::new("x.bat")));
        assert!(!is_batch(Path::new("claude.exe")));
    }

    #[test]
    fn batch_arguments_that_cmd_would_change_are_refused() {
        let script = Path::new(r"C:\Users\John Smith\npm\claude.cmd");
        let check = |arg: &str| check_batch_args(script, &[OsString::from(arg)]).is_ok();
        for ok in [
            "--resume",
            "",
            "fix the bug & test",
            "a|b c",
            "x(y)",
            r"C:\dir\",
            "--x=1",
            "what does /? do",
        ] {
            assert!(check(ok), "{ok:?} should be accepted");
        }
        for bad in [
            "a&b",
            "a|b",
            "<in",
            "say \"hi\"",
            "100%",
            "a ^ b",
            "two\nlines",
            "/?",
            "a/?b",
            "--x=/?",
        ] {
            assert!(!check(bad), "{bad:?} should be refused");
        }
        let odd_script = Path::new(r"C:\Tom&Jerry\claude.cmd");
        assert!(check_batch_args(odd_script, &[]).is_err());
    }
}
