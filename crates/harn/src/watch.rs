use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use harn::config::{Config, Paths};
use harn::instance::single_instance;
use harn::notify::{self, Notifier};
use harn::updates::BackgroundUpdater;
use harn_core::Profile;
use harn_orca::{Event, WatchOptions};

use crate::cli::WatchArgs;

pub fn watch(args: WatchArgs, cfg: Config, paths: &Paths, profiles: &[Profile]) -> Result<i32> {
    let _instance = single_instance(&paths.cache_dir)?;
    let opts = WatchOptions {
        orca_bin: cfg.orca.bin.clone(),
        interval_ms: args.interval_ms.unwrap_or(cfg.orca.interval_ms),
        dry_run: args.dry_run,
        pinned: args.terminals,
    };
    let notifier = Notifier::new(&cfg.notify);
    let mut updates = (cfg.update.enabled && !args.no_update).then(|| {
        let notifier = notifier.clone();
        BackgroundUpdater::spawn(
            cfg.update.clone(),
            paths.cache_dir.clone(),
            move |agent, outcome| {
                if let Some(note) = notify::for_update(agent.id, outcome) {
                    notifier.send(note);
                }
            },
        )
    });
    // harn-orca logs attach/detach/actions itself; the CLI adds notifications and update ticks.
    let mut on_event = |event: &Event| match event {
        Event::Tick { active_agents } => {
            if let Some(updates) = updates.as_mut() {
                updates.on_tick(active_agents);
            }
        }
        Event::Acted {
            agent,
            action,
            dry_run: false,
            ..
        } => {
            if let Some(note) = notify::for_action(agent, action) {
                notifier.send(note);
            }
        }
        _ => {}
    };
    // Ctrl+C simply terminates the process: the watcher holds nothing that needs cleanup.
    let stop = AtomicBool::new(false);
    tracing::info!(
        dry_run = opts.dry_run,
        interval_ms = opts.interval_ms,
        "watching Orca terminals"
    );
    harn_orca::watch(&opts, profiles, &cfg.policy, &mut on_event, &stop)
        .context("cannot watch Orca terminals (is Orca running and `orca` on PATH?)")?;
    Ok(0)
}
