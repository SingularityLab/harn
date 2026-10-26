//! The tray icon and its menu.

use anyhow::{Context, Result};
use tray_icon::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

use crate::icon;
use crate::state::{Colour, Status};

const APP_TITLE: &str = "harn";
/// Windows truncates tray tooltips to 127 UTF-16 units and may drop the terminator at 128.
const TOOLTIP_MAX_CHARS: usize = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    AutoContinue,
    StartAtLogin,
    AutoUpdate,
    Notifications,
    UpdateNow,
    Settings,
    OpenLog,
    Quit,
}

impl Command {
    const ALL: [Self; 8] = [
        Self::AutoContinue,
        Self::StartAtLogin,
        Self::AutoUpdate,
        Self::Notifications,
        Self::UpdateNow,
        Self::Settings,
        Self::OpenLog,
        Self::Quit,
    ];

    fn id(self) -> &'static str {
        match self {
            Self::AutoContinue => "auto-continue",
            Self::StartAtLogin => "start-at-login",
            Self::AutoUpdate => "auto-update",
            Self::Notifications => "notifications",
            Self::UpdateNow => "update-now",
            Self::Settings => "settings",
            Self::OpenLog => "open-log",
            Self::Quit => "quit",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::AutoContinue => "Auto-continue",
            Self::StartAtLogin => "Start at login",
            Self::AutoUpdate => "Auto-update agents",
            Self::Notifications => "Notifications",
            Self::UpdateNow => "Update agents now",
            Self::Settings => "Settings…",
            Self::OpenLog => "Open log",
            Self::Quit => "Quit",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|command| command.id() == id)
    }
}

/// The on/off state behind the check items; the app's state is the source of truth, not the widgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Toggles {
    pub auto_continue: bool,
    pub start_at_login: bool,
    pub auto_update: bool,
    pub notifications: bool,
}

pub struct Ui {
    tray: TrayIcon,
    status: MenuItem,
    last: MenuItem,
    auto_continue: CheckMenuItem,
    start_at_login: CheckMenuItem,
    auto_update: CheckMenuItem,
    notifications: CheckMenuItem,
    shown_colour: Colour,
    shown_status: String,
    shown_last: String,
}

impl Ui {
    pub fn new(status: &Status, last: &str, toggles: Toggles) -> Result<Self> {
        let check = |command: Command, on: bool| {
            CheckMenuItem::with_id(command.id(), command.label(), true, on, None)
        };
        let item = |command: Command| MenuItem::with_id(command.id(), command.label(), true, None);
        let ui_status = MenuItem::new(menu_text(&status.text()), false, None);
        let ui_last = MenuItem::new(menu_text(last), false, None);
        let auto_continue = check(Command::AutoContinue, toggles.auto_continue);
        let start_at_login = check(Command::StartAtLogin, toggles.start_at_login);
        let auto_update = check(Command::AutoUpdate, toggles.auto_update);
        let notifications = check(Command::Notifications, toggles.notifications);
        let menu = Menu::new();
        menu.append_items(&[
            &ui_status,
            &ui_last,
            &PredefinedMenuItem::separator(),
            &auto_continue,
            &start_at_login,
            &auto_update,
            &notifications,
            &PredefinedMenuItem::separator(),
            &item(Command::UpdateNow),
            &item(Command::Settings),
            &item(Command::OpenLog),
            &PredefinedMenuItem::separator(),
            &item(Command::Quit),
        ])
        .context("cannot build the tray menu")?;
        let colour = status.colour();
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(tooltip(status))
            .with_icon(icon::icon(colour)?)
            .build()
            .context("cannot create the tray icon (is a system tray available?)")?;
        Ok(Self {
            tray,
            status: ui_status,
            last: ui_last,
            auto_continue,
            start_at_login,
            auto_update,
            notifications,
            shown_colour: colour,
            shown_status: status.text(),
            shown_last: last.to_owned(),
        })
    }

    /// Touches the OS only for what changed: on Linux every icon change writes a new PNG file.
    pub fn show(&mut self, status: &Status, last: &str) {
        let colour = status.colour();
        if colour != self.shown_colour {
            let changed = icon::icon(colour).and_then(|icon| {
                self.tray
                    .set_icon(Some(icon))
                    .context("cannot change the tray icon")
            });
            match changed {
                Ok(()) => self.shown_colour = colour,
                Err(error) => tracing::warn!("{error:#}"),
            }
        }
        let text = status.text();
        if text != self.shown_status {
            self.status.set_text(menu_text(&text));
            if let Err(error) = self.tray.set_tooltip(Some(tooltip(status))) {
                tracing::warn!(%error, "cannot change the tray tooltip");
            }
            self.shown_status = text;
        }
        if last != self.shown_last {
            self.last.set_text(menu_text(last));
            self.shown_last = last.to_owned();
        }
    }

    /// Always writes every check item: a click has already flipped one, even if applying it failed.
    pub fn sync_checks(&self, toggles: Toggles) {
        self.auto_continue.set_checked(toggles.auto_continue);
        self.start_at_login.set_checked(toggles.start_at_login);
        self.auto_update.set_checked(toggles.auto_update);
        self.notifications.set_checked(toggles.notifications);
    }
}

/// Menus treat `&` as a mnemonic marker.
fn menu_text(text: &str) -> String {
    text.replace('&', "&&")
}

fn tooltip(status: &Status) -> String {
    format!("{APP_TITLE}: {}", status.text())
        .chars()
        .take(TOOLTIP_MAX_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_round_trips_through_its_menu_id() {
        for command in Command::ALL {
            assert_eq!(Command::from_id(command.id()), Some(command));
        }
        assert_eq!(Command::from_id("nope"), None);
    }

    #[test]
    fn tooltips_stay_within_the_windows_limit() {
        let status = Status::Stopped {
            reason: "x".repeat(500),
        };
        assert_eq!(tooltip(&status).chars().count(), TOOLTIP_MAX_CHARS);
        assert_eq!(tooltip(&Status::Paused), "harn: Paused");
    }

    #[test]
    fn ampersands_are_shown_literally() {
        assert_eq!(menu_text("a & b"), "a && b");
    }
}
