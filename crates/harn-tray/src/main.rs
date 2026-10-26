#![windows_subsystem = "windows"]

mod app;
mod autostart;
mod cli;
mod icon;
mod settings;
mod state;
mod ui;
mod watcher;

use clap::Parser;

const EXIT_FAILURE: i32 = 1;

fn main() {
    let cli = cli::Cli::try_parse().unwrap_or_else(|error| {
        // A GUI app has no console on Windows: without a popup a typo would fail silently.
        if error.use_stderr() {
            app::popup("harn-tray: invalid arguments", error.to_string());
        }
        error.exit()
    });
    if let Err(error) = app::run(cli) {
        tracing::error!("{error:#}");
        app::popup("harn-tray cannot start", format!("{error:#}"));
        std::process::exit(EXIT_FAILURE);
    }
}
