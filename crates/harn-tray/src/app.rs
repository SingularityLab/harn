//! The event loop: wires the watcher thread, the updater and the menu to the tray state.

mod control;

use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use harn::config::{self, Config, Paths};
use harn::instance::single_instance;
use harn::logging;
use harn::notify::{self, Notifier, NotifyConfig};
use harn::updates::BackgroundUpdater;
use harn_orca::TerminalPin;
use harn_update::{Outcome, UpdateConfig};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tray_icon::TrayIconEvent;
use tray_icon::menu::MenuEvent;

use crate::autostart::Autostart;
use crate::cli::Cli;
use crate::state::{Report, TrayState};
use crate::ui::{Command, Toggles, Ui};
use crate::watcher::Watcher;

/// Background update checks also run while Orca is closed, using the last known live sessions.
const UPDATE_TICK: Duration = Duration::from_secs(5 * 60);
/// How often the tray picks up config edits and notices a watcher that went quiet.
const HOUSEKEEPING: Duration = Duration::from_secs(5);
const CLOCK_FORMAT: &str = "%H:%M";
const EXIT_FAILURE: i32 = 1;

pub enum UserEvent {
    Menu(MenuEvent),
    Tray(TrayIconEvent),
    /// `generation` tells a stopped watcher's still-queued reports from the current watcher's.
    Watch {
        generation: u64,
        report: Report,
    },
    Updated {
        agent: &'static str,
        outcome: Outcome,
    },
}

pub fn run(cli: Cli) -> Result<()> {
    let paths = Paths::resolve(cli.config.clone())?;
    init_logging(&paths.log_file);
    let instance = match single_instance(&paths.cache_dir) {
        Ok(file) => file,
        Err(error) => {
            report_lock_failure(&error);
            return Ok(());
        }
    };
    let cfg = config::load(&paths.config_file)?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %paths.config_file.display(),
        "harn-tray starting"
    );
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    hide_from_dock(&mut event_loop);
    forward_ui_events(&event_loop);
    let mut app = App::new(paths, cfg, cli, event_loop.create_proxy(), instance);
    event_loop.run(move |event, _, control_flow| app.on_loop_event(event, control_flow))
}

/// For messages that must show even with notifications off: they answer the user starting the app.
pub fn popup(title: &str, body: String) {
    Notifier::new(&NotifyConfig::default()).send((title.to_owned(), body));
}

fn init_logging(log_file: &Path) {
    if let Err(error) = logging::to_file(log_file) {
        popup("harn-tray: logging is off", format!("{error:#}"));
    }
    std::panic::set_hook(Box::new(|info| tracing::error!("panic: {info}")));
}

fn report_lock_failure(error: &anyhow::Error) {
    tracing::warn!("{error:#}");
    // A lock held by another watcher carries no IO error; anything else is a real failure.
    let broken = error.chain().any(|cause| cause.is::<std::io::Error>());
    let title = if broken {
        "harn-tray cannot start"
    } else {
        "harn is already running"
    };
    popup(title, format!("{error:#}"));
}

#[cfg(target_os = "macos")]
fn hide_from_dock(event_loop: &mut EventLoop<UserEvent>) {
    use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
}

#[cfg(not(target_os = "macos"))]
fn hide_from_dock(_event_loop: &mut EventLoop<UserEvent>) {}

fn forward_ui_events(event_loop: &EventLoop<UserEvent>) {
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event| send(&proxy, UserEvent::Menu(event))));
    let proxy = event_loop.create_proxy();
    TrayIconEvent::set_event_handler(Some(move |event| send(&proxy, UserEvent::Tray(event))));
}

fn send(proxy: &EventLoopProxy<UserEvent>, event: UserEvent) {
    if proxy.send_event(event).is_err() {
        tracing::debug!("the event loop has exited; dropping an event");
    }
}

/// Always running, so "Update agents now" works with auto-update off; the menu toggle only gates
/// background checks. Interval changes apply on the next start: replacing the worker mid-install
/// could run two installers for one agent at once.
fn spawn_updater(
    cfg: &UpdateConfig,
    paths: &Paths,
    proxy: &EventLoopProxy<UserEvent>,
) -> BackgroundUpdater {
    let proxy = proxy.clone();
    let cfg = UpdateConfig {
        enabled: true,
        ..cfg.clone()
    };
    BackgroundUpdater::spawn(cfg, paths.cache_dir.clone(), move |agent, outcome| {
        let outcome = outcome.clone();
        send(
            &proxy,
            UserEvent::Updated {
                agent: agent.id,
                outcome,
            },
        );
    })
}

struct App {
    paths: Paths,
    cfg: Config,
    config_stamp: Option<SystemTime>,
    pins: Vec<TerminalPin>,
    proxy: EventLoopProxy<UserEvent>,
    notifier: Notifier,
    state: TrayState,
    watcher: Option<Watcher>,
    generation: u64,
    updater: BackgroundUpdater,
    autostart: Option<Autostart>,
    ui: Option<Ui>,
    next_update_tick: Instant,
    next_housekeeping: Instant,
    /// Held for the life of the process: it keeps a second watcher from starting.
    _instance: File,
}

impl App {
    fn new(
        paths: Paths,
        cfg: Config,
        cli: Cli,
        proxy: EventLoopProxy<UserEvent>,
        instance: File,
    ) -> Self {
        let autostart = Autostart::new(cli.config.as_deref())
            .inspect_err(|error| tracing::warn!("{error:#}"))
            .ok();
        let now = Instant::now();
        Self {
            config_stamp: modified(&paths.config_file),
            notifier: Notifier::new(&cfg.notify),
            state: TrayState::new(Duration::from_millis(cfg.orca.interval_ms)),
            updater: spawn_updater(&cfg.update, &paths, &proxy),
            pins: cli.terminals,
            watcher: None,
            generation: 0,
            autostart,
            ui: None,
            next_update_tick: now + UPDATE_TICK,
            next_housekeeping: now + HOUSEKEEPING,
            _instance: instance,
            paths,
            cfg,
            proxy,
        }
    }

    fn on_loop_event(&mut self, event: Event<'_, UserEvent>, control_flow: &mut ControlFlow) {
        match event {
            // macOS can only create the tray icon once the event loop is running.
            Event::NewEvents(StartCause::Init) => {
                if let Err(error) = self.start() {
                    tracing::error!("{error:#}");
                    popup("harn-tray cannot start", format!("{error:#}"));
                    self.shut_down();
                    *control_flow = ControlFlow::ExitWithCode(EXIT_FAILURE);
                    return;
                }
            }
            Event::NewEvents(_) => self.housekeeping_if_due(),
            Event::UserEvent(event) => {
                if !self.handle(event) {
                    self.shut_down();
                    *control_flow = ControlFlow::Exit;
                    return;
                }
            }
            _ => {}
        }
        *control_flow = ControlFlow::WaitUntil(self.next_update_tick.min(self.next_housekeeping));
    }

    fn start(&mut self) -> Result<()> {
        self.start_watcher();
        let status = self.state.status(Instant::now());
        self.ui = Some(Ui::new(&status, &self.state.last_line(), self.toggles())?);
        tracing::info!("tray icon created");
        // Only once the tray works: a desktop without one must not get a failing app at every login.
        self.enable_autostart_on_first_run();
        self.sync_checks();
        Ok(())
    }

    fn shut_down(&mut self) {
        if let Some(watcher) = self.watcher.take() {
            watcher.stop();
        }
        // Dropping the icon removes it right away; Windows would leave a ghost until hovered.
        self.ui = None;
        tracing::info!("harn-tray stopped");
    }

    /// Returns false when the user chose Quit.
    fn handle(&mut self, event: UserEvent) -> bool {
        match event {
            UserEvent::Menu(event) => {
                self.state.acknowledge();
                match Command::from_id(event.id().as_ref()) {
                    Some(Command::Quit) => return false,
                    Some(command) => self.run_command(command),
                    None => tracing::warn!(id = ?event.id(), "unknown menu item"),
                }
            }
            UserEvent::Tray(TrayIconEvent::Click { .. }) => self.state.acknowledge(),
            UserEvent::Tray(_) => return true,
            UserEvent::Watch { generation, report } if generation == self.generation => {
                self.on_report(report);
            }
            UserEvent::Watch { .. } => return true,
            UserEvent::Updated { agent, outcome } => self.on_update(agent, &outcome),
        }
        self.refresh();
        true
    }

    fn on_report(&mut self, report: Report) {
        match &report {
            Report::Event(harn_orca::Event::Tick { active_agents }) if self.cfg.update.enabled => {
                self.updater.on_tick(active_agents);
            }
            Report::Event(harn_orca::Event::Acted {
                agent,
                action,
                dry_run: false,
                ..
            }) => {
                if let Some(note) = notify::for_action(agent, action) {
                    self.notifier.send(note);
                }
            }
            Report::Stopped(reason) => self
                .notifier
                .send(("harn stopped watching".to_owned(), reason.clone())),
            _ => {}
        }
        self.state.apply(report, Instant::now(), &clock());
    }

    fn on_update(&mut self, agent: &str, outcome: &Outcome) {
        if let Some(note) = notify::for_update(agent, outcome) {
            self.notifier.send(note);
        }
        self.state.record_update(agent, outcome, &clock());
    }

    fn refresh(&mut self) {
        let status = self.state.status(Instant::now());
        let last = self.state.last_line();
        if let Some(ui) = &mut self.ui {
            ui.show(&status, &last);
        }
    }

    fn sync_checks(&self) {
        if let Some(ui) = &self.ui {
            ui.sync_checks(self.toggles());
        }
    }

    fn toggles(&self) -> Toggles {
        let start_at_login = self.autostart.as_ref().is_some_and(|autostart| {
            autostart
                .is_enabled()
                .inspect_err(|error| tracing::warn!("{error:#}"))
                .unwrap_or(false)
        });
        Toggles {
            auto_continue: self.watcher.is_some(),
            start_at_login,
            auto_update: self.cfg.update.enabled,
            notifications: self.cfg.notify.enabled,
        }
    }
}

fn clock() -> String {
    jiff::Zoned::now().strftime(CLOCK_FORMAT).to_string()
}

/// Missing or unreadable reads as "no stamp"; `config::load` reports the actual problem.
fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|meta| meta.modified()).ok()
}
