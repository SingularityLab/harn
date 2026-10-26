//! What the menu items do, and how config edits are picked up.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use harn::config::{self, Config};
use harn::notify::Notifier;
use harn_orca::WatchOptions;

use super::{App, HOUSEKEEPING, UPDATE_TICK, UserEvent, clock, modified, send};
use crate::autostart;
use crate::settings;
use crate::state::Report;
use crate::ui::Command;
use crate::watcher::Watcher;

impl App {
    pub(super) fn run_command(&mut self, command: Command) {
        let done = match command {
            Command::AutoContinue => {
                self.toggle_watcher();
                Ok(())
            }
            Command::StartAtLogin => self.toggle_autostart(),
            Command::AutoUpdate => self.set_flag("update", !self.cfg.update.enabled),
            Command::Notifications => self.set_flag("notify", !self.cfg.notify.enabled),
            Command::UpdateNow => {
                self.notifier
                    .send(("harn".to_owned(), "Checking agents for updates…".to_owned()));
                self.updater.update_now(self.state.active_agents());
                Ok(())
            }
            Command::Settings => settings::open_config(&self.paths.config_file),
            Command::OpenLog => settings::open_file(&self.paths.log_file),
            Command::Quit => Ok(()),
        };
        if let Err(error) = done {
            tracing::error!("{}: {error:#}", command.label());
            let title = format!("harn: {} failed", command.label());
            self.notifier.send((title, format!("{error:#}")));
        }
        self.sync_checks();
    }

    fn toggle_watcher(&mut self) {
        match self.watcher.take() {
            Some(watcher) => {
                watcher.stop();
                self.state.paused();
                tracing::info!("auto-continue paused");
            }
            None => self.start_watcher(),
        }
    }

    pub(super) fn start_watcher(&mut self) {
        self.generation += 1;
        self.state
            .started(Duration::from_millis(self.cfg.orca.interval_ms));
        let generation = self.generation;
        let proxy = self.proxy.clone();
        let report = move |report| send(&proxy, UserEvent::Watch { generation, report });
        let opts = WatchOptions {
            orca_bin: self.cfg.orca.bin.clone(),
            interval_ms: self.cfg.orca.interval_ms,
            dry_run: false,
            pinned: self.pins.clone(),
        };
        let started = config::load_profiles(&self.paths.profiles_dir).and_then(|profiles| {
            let profiles = profiles.into_iter().map(|loaded| loaded.profile).collect();
            Watcher::spawn(opts, profiles, self.cfg.policy.clone(), report)
        });
        match started {
            Ok(watcher) => {
                tracing::info!(
                    interval_ms = self.cfg.orca.interval_ms,
                    "watching Orca terminals"
                );
                self.watcher = Some(watcher);
            }
            Err(error) => {
                tracing::error!("cannot start watching: {error:#}");
                let reason = format!("{error:#}");
                self.notifier
                    .send(("harn stopped watching".to_owned(), reason.clone()));
                self.state
                    .apply(Report::Stopped(reason), Instant::now(), &clock());
            }
        }
    }

    fn restart_watcher(&mut self) {
        if let Some(watcher) = self.watcher.take() {
            watcher.stop();
            self.start_watcher();
        }
    }

    fn toggle_autostart(&self) -> Result<()> {
        let autostart = self
            .autostart
            .as_ref()
            .context("start at login is not supported on this system")?;
        autostart.set(!autostart.is_enabled()?)
    }

    pub(super) fn enable_autostart_on_first_run(&self) {
        let (Some(autostart), Some(dir)) = (&self.autostart, self.paths.config_file.parent())
        else {
            return;
        };
        match autostart::on_first_run(dir, || autostart.set(true)) {
            Ok(true) => tracing::info!("first run: start at login turned on"),
            Ok(false) => {}
            Err(error) => tracing::warn!("cannot turn on start at login: {error:#}"),
        }
    }

    fn set_flag(&mut self, table: &str, enabled: bool) -> Result<()> {
        config::set_bool(&self.paths.config_file, table, "enabled", enabled)?;
        let cfg = config::load(&self.paths.config_file)?;
        self.config_stamp = modified(&self.paths.config_file);
        self.apply_config(cfg);
        tracing::info!(table, enabled, "setting changed from the menu");
        Ok(())
    }

    fn apply_config(&mut self, cfg: Config) {
        self.notifier = Notifier::new(&cfg.notify);
        self.cfg = cfg;
    }

    pub(super) fn housekeeping_if_due(&mut self) {
        let now = Instant::now();
        if now >= self.next_update_tick {
            self.next_update_tick = now + UPDATE_TICK;
            if self.cfg.update.enabled {
                self.updater.on_tick(self.state.active_agents());
            }
        }
        if now >= self.next_housekeeping {
            self.next_housekeeping = now + HOUSEKEEPING;
            self.reload_if_config_changed();
            self.refresh();
        }
    }

    fn reload_if_config_changed(&mut self) {
        let stamp = modified(&self.paths.config_file);
        if stamp == self.config_stamp {
            return;
        }
        // Remembered before loading, so a broken file is reported once, not every few seconds.
        self.config_stamp = stamp;
        match config::load(&self.paths.config_file) {
            Ok(cfg) => {
                tracing::info!("config changed; applying it");
                self.apply_config(cfg);
                self.restart_watcher();
                self.sync_checks();
            }
            Err(error) => {
                tracing::error!("config not applied: {error:#}");
                let title = "harn: settings not applied".to_owned();
                self.notifier.send((title, format!("{error:#}")));
            }
        }
    }
}
