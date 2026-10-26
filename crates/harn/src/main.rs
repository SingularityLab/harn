mod cli;
mod commands;
mod config;
mod logging;
mod watch;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};
use crate::config::Paths;

const EXIT_ERROR: i32 = 2;

fn main() {
    let cli = Cli::parse();
    let code = match real_main(cli) {
        Ok(code) => code,
        Err(e) => {
            tracing::error!("{e:#}");
            eprintln!("harn: {e:#}");
            EXIT_ERROR
        }
    };
    // Also ends a `run` session whose stdin thread is still blocked in a console read.
    std::process::exit(code);
}

fn real_main(cli: Cli) -> Result<i32> {
    let paths = Paths::resolve(cli.config)?;
    if matches!(cli.command, Command::Run(_)) {
        if let Err(e) = logging::to_file(&paths.log_file) {
            eprintln!("harn: logging disabled: {e:#}");
        }
        // The default hook prints over the agent's TUI; harn-pty restores the terminal while unwinding.
        std::panic::set_hook(Box::new(|info| tracing::error!("panic: {info}")));
    } else if matches!(cli.command, Command::Watch(_)) {
        logging::to_stderr(logging::DEFAULT_LEVEL);
    } else {
        logging::to_stderr(logging::QUIET_LEVEL);
    }
    let cfg = config::load(&paths.config_file)?;
    let profiles = config::load_profiles(&paths.profiles_dir)?;
    match cli.command {
        Command::Run(args) => commands::run(args, cfg, &paths, &profiles),
        Command::Watch(args) => {
            let profiles: Vec<_> = profiles.into_iter().map(|p| p.profile).collect();
            watch::watch(args, cfg, &paths, &profiles)
        }
        Command::Update(args) => commands::update(args, cfg, &paths),
        Command::Check(args) => commands::check(args, &profiles),
        Command::Profiles => commands::list_profiles(&profiles),
    }
}
