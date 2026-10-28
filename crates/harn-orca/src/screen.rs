//! What a screen says about the agent: a shell prompt at the bottom means it exited (Orca keeps
//! naming the terminal after it, with its last error still visible); the input row, found with the
//! profile's `user_echo`/`empty_input` signatures, says what typing would land in.

use harn_core::Profile;

/// True if the bottom non-blank line looks like an interactive shell prompt.
pub(crate) fn shows_shell_prompt(screen: &str) -> bool {
    let Some(line) = screen.lines().map(str::trim).rfind(|l| !l.is_empty()) else {
        return false;
    };
    let mut chars = line.chars();
    // `PS C:\src>` / `C:\src>`, possibly followed by a half-typed command.
    let windows = line.starts_with("PS>")
        || line.starts_with("PS ")
        || matches!(
            (chars.next(), chars.next(), chars.next()),
            (Some(d), Some(':'), Some('\\')) if d.is_ascii_alphabetic()
        );
    // `user@host:~/src$`, `bash-5.2$`, `root#`, `host %`, fish `~/src>`, starship `❯`.
    let mut rev = line.chars().rev();
    let posix = match rev.next() {
        Some('$' | '#' | '>' | '❯' | 'λ' | '»') => true,
        // `ctx: 18%` in an agent's status line is not a zsh prompt.
        Some('%') => !rev.next().is_some_and(|c| c.is_ascii_digit()),
        _ => false,
    };
    (windows && line.contains('>')) || posix
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputRow {
    /// Nothing looks like the agent's input box: keys would land somewhere unknown.
    Missing,
    Empty,
    /// The text in the input box, trimmed.
    Holds(String),
}

/// The last row echoing user input is the live input box; it is empty when it matches
/// `empty_input` (a bare prompt or a placeholder) or holds nothing after the prompt.
pub(crate) fn input_row(profile: &Profile, screen: &str) -> InputRow {
    let sigs = &profile.signatures;
    let found = screen.lines().rev().find_map(|row| {
        let prompt = sigs.user_echo.iter().find_map(|p| p.0.find(row))?;
        Some((row, prompt.end()))
    });
    let Some((row, prompt_end)) = found else {
        return InputRow::Missing;
    };
    let text = row.get(prompt_end..).unwrap_or_default().trim();
    if text.is_empty() || sigs.empty_input.iter().any(|p| p.is_match(row)) {
        InputRow::Empty
    } else {
        InputRow::Holds(text.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    fn capture(json: &str) -> String {
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        let rows = value["result"]["terminal"]["tail"].as_array().unwrap();
        let rows: Vec<&str> = rows.iter().filter_map(|r| r.as_str()).collect();
        rows.join("\n")
    }

    /// Every `.txt` capture under `dir` at any depth (`claude/`, `synthetic/codex/`, ...).
    fn screen_files(dir: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else if path.extension().is_some_and(|ext| ext == "txt") {
                    files.push(path);
                }
            }
        }
        files
    }

    #[test]
    fn shell_prompts_at_the_bottom() {
        let prompts = [
            r"PS C:\src> claude",
            "PS>",
            r"C:\Users\alice>",
            "dex@box:~/src$",
            "$",
        ];
        let more = ["root@box:/#", "dex@mac ~ %", "~/src>", "❯", ">>>"];
        for bottom in prompts.into_iter().chain(more) {
            assert!(
                shows_shell_prompt(&format!("API Error: 529\n{bottom}\n")),
                "{bottom:?}"
            );
        }
        let agents = [
            "  ? for shortcuts",
            "  Opus | 5h: 2% · ctx: 18%",
            "› Ask Codex",
            "────",
        ];
        for bottom in agents {
            assert!(!shows_shell_prompt(&format!("❯\n{bottom}")), "{bottom:?}");
        }
        let exited = capture(include_str!("../tests/data/read_screen_exited.json"));
        assert!(exited.contains("● API Error: 529") && shows_shell_prompt(&exited));
    }

    /// Captures of agents that exited (`exited_*`) end in a shell prompt; every other capture is a
    /// live agent screen and must never look like one.
    #[test]
    fn real_captures_tell_exited_agents_from_live_ones() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../harn-core/tests/fixtures");
        let files = screen_files(&root);
        let mut exited = 0;
        for path in &files {
            let screen = std::fs::read_to_string(path).unwrap();
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let is_exited = name.starts_with("exited");
            exited += usize::from(is_exited);
            assert_eq!(shows_shell_prompt(&screen), is_exited, "{}", path.display());
        }
        assert!(
            exited > 0 && files.len() > exited,
            "captures under {}",
            root.display()
        );
    }

    #[test]
    fn input_row_from_profile_signatures() {
        let profiles = Profile::builtins();
        let claude = profiles.iter().find(|p| p.id == "claude").unwrap();
        let agent = capture(include_str!("../tests/data/read_screen_agent.json"));
        let typed = capture(include_str!("../tests/data/read_screen_typed.json"));
        assert_eq!(input_row(claude, &agent), InputRow::Empty);
        assert_eq!(
            input_row(claude, &typed),
            InputRow::Holds("continue".to_owned())
        );
        assert_eq!(
            input_row(claude, "● Working\n  esc to interrupt"),
            InputRow::Missing
        );
        // An exited agent's last frame keeps its input box: the shell guard has to run first.
        let exited = capture(include_str!("../tests/data/read_screen_exited.json"));
        assert_eq!(input_row(claude, &exited), InputRow::Empty);
    }
}
