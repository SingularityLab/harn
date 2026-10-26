//! How an agent was installed, which command upgrades it, and reading versions.

use std::fs;
use std::path::{Path, PathBuf};

use semver::Version;

use crate::Agent;
use crate::system::Cmd;

/// `Npm` knows the package folder holding `package.json`; `ClaudeNative` is Claude Code's own
/// installer (`~/.local/bin`, or the older `~/.claude/local`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InstallMethod {
    Npm { package_dir: PathBuf },
    ClaudeNative,
    Homebrew,
}

/// Matched against the lowercased, `/`-separated path. Casks (Claude Code is one) live in Caskroom.
const HOMEBREW_MARKERS: [&str; 4] = ["/cellar/", "/caskroom/", "/homebrew/", "/linuxbrew/"];
const CLAUDE_NATIVE_DIRS: [&str; 2] = [".local/bin", ".claude/local"];
const PACKAGE_JSON: &str = "package.json";

/// `None` when harn does not know how to update this install.
pub(crate) fn detect(agent: &Agent, bin: &Path, home: Option<&Path>) -> Option<InstallMethod> {
    // npm goes first: a Homebrew node keeps its npm globals under the Homebrew prefix.
    if let Some(package_dir) = npm_package_dir(agent, bin) {
        Some(InstallMethod::Npm { package_dir })
    } else if is_homebrew(bin) {
        Some(InstallMethod::Homebrew)
    } else if agent.id == "claude" && is_claude_native(bin, home) {
        Some(InstallMethod::ClaudeNative)
    } else {
        None
    }
}

/// npm puts the shim and the package side by side: `<prefix>\claude.cmd` + `<prefix>\node_modules\<pkg>`
/// on Windows, `<prefix>/bin/claude` + `<prefix>/lib/node_modules/<pkg>` elsewhere.
fn npm_package_dir(agent: &Agent, bin: &Path) -> Option<PathBuf> {
    let bin_dir = bin.parent()?;
    let unix_root = bin_dir.parent().map(|prefix| prefix.join("lib"));
    [Some(bin_dir.to_path_buf()), unix_root]
        .into_iter()
        .flatten()
        .map(|root| root.join("node_modules").join(agent.npm_package))
        .find(|dir| dir.join(PACKAGE_JSON).is_file())
}

/// Homebrew links `bin/<name>` into a Cellar/Caskroom keg, so on Intel macOS (`/usr/local/bin`) only
/// the symlink target reveals it.
fn is_homebrew(bin: &Path) -> bool {
    let mentions = |path: &Path| {
        let path = normalize(path);
        HOMEBREW_MARKERS.iter().any(|marker| path.contains(marker))
    };
    mentions(bin) || fs::canonicalize(bin).is_ok_and(|real| mentions(&real))
}

fn is_claude_native(bin: &Path, home: Option<&Path>) -> bool {
    let Some(home) = home else { return false };
    let (bin, home) = (normalize(bin), normalize(home));
    let home = home.trim_end_matches('/');
    CLAUDE_NATIVE_DIRS
        .iter()
        .any(|dir| bin.starts_with(&format!("{home}/{dir}/")))
}

/// Windows paths differ in case and separators between PATH, `dirs` and `canonicalize`.
fn normalize(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase()
}

impl InstallMethod {
    /// The command that upgrades `agent`. `bin` is the binary that was found, so a self-update targets
    /// that install rather than another one on PATH.
    pub(crate) fn upgrade_cmd(&self, agent: &Agent, bin: &Path) -> Cmd {
        match self {
            Self::Npm { .. } if agent.id != "claude" => Cmd::new(
                "npm",
                &["i", "-g", &format!("{}@latest", agent.npm_package)],
            ),
            // Claude ships an updater that handles both npm and native installs.
            Self::Npm { .. } | Self::ClaudeNative => Cmd::new(bin, &["update"]),
            Self::Homebrew => Cmd::new("brew", &["upgrade", brew_name(agent)]),
        }
    }
}

/// `claude` is the desktop app cask; Claude Code is `claude-code`.
fn brew_name(agent: &Agent) -> &'static str {
    match agent.id {
        "claude" => "claude-code",
        "gemini" => "gemini-cli",
        _ => agent.binary,
    }
}

/// First `x.y.z[-pre]` in `text`, e.g. `2.1.310 (Claude Code)` or `codex-cli 0.165.0-alpha.1`.
pub(crate) fn extract_version(text: &str) -> Option<Version> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+')))
        .find_map(|token| {
            Version::parse(token.trim_start_matches('v').trim_end_matches(['.', '-'])).ok()
        })
}

/// Version from `<package_dir>/package.json`.
pub(crate) fn package_version(package_dir: &Path) -> Option<Version> {
    let text = fs::read_to_string(package_dir.join(PACKAGE_JSON)).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    Version::parse(json["version"].as_str()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::find;
    use crate::testutil::TempDir;

    fn found(agent: &str, bin: impl AsRef<Path>, home: Option<&str>) -> Option<InstallMethod> {
        detect(find(agent).unwrap(), bin.as_ref(), home.map(Path::new))
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn npm_global_layouts_are_detected_in_the_windows_and_unix_shapes() {
        let root = TempDir::new("npm");
        let windows = root.write("npm/codex.cmd", "");
        root.write("npm/node_modules/@openai/codex/package.json", "{}");
        let unix = root.write("prefix/bin/opencode", "");
        root.write("prefix/lib/node_modules/opencode-ai/package.json", "{}");
        let lone_shim = root.write("lone/bin/codex", "");
        let project = root.write("proj/node_modules/.bin/codex", "");
        root.write("proj/node_modules/@openai/codex/package.json", "{}");

        let npm = |dir: &str| {
            let package_dir = root.join(dir);
            Some(InstallMethod::Npm { package_dir })
        };
        let (codex_dir, opencode_dir) = (
            "npm/node_modules/@openai/codex",
            "prefix/lib/node_modules/opencode-ai",
        );
        assert_eq!(found("codex", windows, None), npm(codex_dir));
        assert_eq!(found("opencode", unix, None), npm(opencode_dir));
        assert!(
            found("codex", lone_shim, None).is_none(),
            "a shim without its package"
        );
        assert!(
            found("codex", project, None).is_none(),
            "a project-local install"
        );
    }

    #[test]
    fn homebrew_is_recognised_by_path_but_npm_under_a_brew_prefix_stays_npm() {
        let brew = Some(InstallMethod::Homebrew);
        for bin in [
            "/opt/homebrew/bin/codex",
            "/usr/local/Cellar/codex/0.164.0/bin/codex",
            "/usr/local/Caskroom/claude-code/2.1.310/claude",
            "/home/linuxbrew/.linuxbrew/bin/codex",
        ] {
            assert_eq!(found("codex", bin, None), brew, "{bin}");
        }
        let root = TempDir::new("brewnpm");
        let bin = root.write("linuxbrew/.linuxbrew/bin/codex", "");
        root.write(
            "linuxbrew/.linuxbrew/lib/node_modules/@openai/codex/package.json",
            "{}",
        );
        assert!(matches!(
            found("codex", bin, None),
            Some(InstallMethod::Npm { .. })
        ));
    }

    #[test]
    fn claude_native_installs_live_under_the_home_directory() {
        let native = Some(InstallMethod::ClaudeNative);
        let at = |bin: &str, home: &str| found("claude", bin, Some(home));
        assert_eq!(at("/home/dex/.local/bin/claude", "/home/dex"), native);
        assert_eq!(at("/home/dex/.claude/local/claude", "/home/dex"), native);
        // Case and separators differ between PATH and the home directory on Windows.
        assert_eq!(
            at(r"c:\USERS\dex\.local\bin\claude.exe", r"C:\Users\dex"),
            native
        );
        assert!(
            at("/home/me/.local/bin/claude", "/home/dex").is_none(),
            "someone else's home"
        );
        assert!(found("codex", "/home/dex/.local/bin/codex", Some("/home/dex")).is_none());
    }

    #[test]
    fn each_install_method_has_its_own_upgrade_command() {
        let npm = InstallMethod::Npm {
            package_dir: PathBuf::new(),
        };
        let cmd = |method: &InstallMethod, agent: &str| {
            let cmd = method.upgrade_cmd(find(agent).unwrap(), Path::new("/bin/agent"));
            cmd.to_string()
        };
        assert_eq!(cmd(&npm, "codex"), "npm i -g @openai/codex@latest");
        assert_eq!(cmd(&npm, "gemini"), "npm i -g @google/gemini-cli@latest");
        assert_eq!(cmd(&npm, "claude"), "agent update");
        assert_eq!(cmd(&InstallMethod::ClaudeNative, "claude"), "agent update");
        let brew = InstallMethod::Homebrew;
        assert_eq!(cmd(&brew, "claude"), "brew upgrade claude-code");
        assert_eq!(cmd(&brew, "codex"), "brew upgrade codex");
    }

    #[test]
    fn versions_are_extracted_from_cli_output_and_package_json() {
        for (text, want) in [
            ("2.1.310 (Claude Code)\n", "2.1.310"),
            ("codex-cli 0.164.0\n", "0.164.0"),
            ("v1.2.3", "1.2.3"),
            ("opencode version 0.15.3.", "0.15.3"),
            ("codex-cli 0.165.0-alpha.3 (abc)", "0.165.0-alpha.3"),
            ("node 22.1.0, app 9.9.9", "22.1.0"),
        ] {
            assert_eq!(extract_version(text), Some(v(want)), "{text:?}");
        }
        for text in ["", "no digits", "1.2", "v1", "01.2.3", "2026-10-26"] {
            assert_eq!(extract_version(text), None, "{text:?}");
        }

        let dir = TempDir::new("pkg");
        dir.write("package.json", r#"{"name":"x","version":"0.164.0"}"#);
        assert_eq!(package_version(&dir), Some(v("0.164.0")));
        for broken in ["", "not json", r#"{"name":"x"}"#, r#"{"version":"latest"}"#] {
            dir.write("package.json", broken);
            assert_eq!(package_version(&dir), None, "{broken:?}");
        }
    }
}
