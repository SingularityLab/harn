use std::path::PathBuf;

use clap::Parser;
use harn_orca::TerminalPin;

#[derive(Debug, Parser)]
#[command(
    name = "harn-tray",
    version,
    about = "Tray app for harn: keeps Claude Code, Codex and other terminal agents running"
)]
pub struct Cli {
    /// Config file (default: <config dir>/harn/config.toml).
    #[arg(long, env = "HARN_CONFIG")]
    pub config: Option<PathBuf>,

    /// Watch only this terminal as this agent, e.g. `term_1234=claude` (repeatable). Useful for
    /// agents Orca does not identify. Handles come from `orca terminal list`.
    #[arg(long = "terminal", value_name = "HANDLE=AGENT", value_parser = parse_pin)]
    pub terminals: Vec<TerminalPin>,
}

fn parse_pin(src: &str) -> Result<TerminalPin, String> {
    match src.split_once('=') {
        Some((handle, agent)) if !handle.trim().is_empty() && !agent.trim().is_empty() => {
            Ok(TerminalPin {
                handle: handle.trim().to_owned(),
                agent: agent.trim().to_owned(),
            })
        }
        _ => Err(format!("expected HANDLE=AGENT, got `{src}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config_and_repeated_pins() {
        let cli = Cli::try_parse_from([
            "harn-tray",
            "--config",
            "my.toml",
            "--terminal",
            "term_1= claude",
            "--terminal",
            "term_2=codex",
        ])
        .unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("my.toml")));
        let agents: Vec<_> = cli.terminals.iter().map(|p| p.agent.as_str()).collect();
        assert_eq!(agents, ["claude", "codex"]);
        assert_eq!(cli.terminals[0].handle, "term_1");
    }

    #[test]
    fn rejects_malformed_pins() {
        for bad in ["term_1", "=claude", "term_1=", " = "] {
            assert!(parse_pin(bad).is_err(), "{bad}");
        }
    }
}
