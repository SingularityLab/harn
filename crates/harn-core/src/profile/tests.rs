use super::*;

#[test]
fn builtins_parse() {
    for src in BUILTIN_SOURCES {
        if let Err(e) = Profile::from_toml(src) {
            panic!("built-in profile failed to parse: {e}");
        }
    }
    let ids: Vec<_> = Profile::builtins().into_iter().map(|p| p.id).collect();
    assert_eq!(ids, ["claude", "codex", "gemini", "opencode"]);
}

#[test]
fn matches_command_names_and_identities() {
    let [claude, codex, gemini, opencode]: [Profile; 4] = Profile::builtins().try_into().unwrap();
    let selects = r"claude|Claude|claude.exe|CLAUDE.CMD|claude.ps1|claude.bat|/usr/local/bin/claude|C:\npm\claude.cmd|  claude  |claude-code|Claude Code";
    for name in selects.split('|') {
        assert!(claude.matches(name), "{name:?} should select claude");
    }
    for name in "|claudex|claude.sh|codex|/usr/bin/|claude.exe.bak".split('|') {
        assert!(!claude.matches(name), "{name:?} should not select claude");
    }
    assert!(codex.matches(r"D:\tools\codex.exe"));
    assert!(gemini.matches("gemini") && opencode.matches("opencode.cmd"));
}

#[test]
fn loading_defaults_and_errors() {
    let p = Profile::from_toml("id = \"x\"\nname = \"X\"\n[signatures]\n").unwrap();
    let keys = (
        p.continue_text.as_str(),
        p.submit.as_str(),
        p.interrupt.as_str(),
    );
    assert_eq!(keys, ("continue", "\r", "\u{1b}"));
    assert_eq!((p.gap_ms, p.tail_lines), (150, 15));
    let bad_regex = "id = \"x\"\nname = \"X\"\n[signatures]\nworking = ['(unclosed']\n";
    let err = Profile::from_toml(bad_regex).unwrap_err().to_string();
    assert!(
        err.contains("invalid profile TOML") && err.contains("unclosed"),
        "{err}"
    );
    for typo in ["typo = 1\n[signatures]\n", "[signatures]\nwroking = []\n"] {
        let src = format!("id = \"x\"\nname = \"X\"\n{typo}");
        assert!(Profile::from_toml(&src).is_err(), "{typo}");
    }
}
