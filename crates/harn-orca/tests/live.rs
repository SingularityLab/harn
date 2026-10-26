//! Live smoke test against a running Orca. Only ever point it at a terminal you created yourself,
//! running a fake agent that shows `● API Error: 529 ...` above a `❯` input box and logs its stdin
//! as `received: "..."`; then `HARN_ORCA_LIVE_HANDLE=term_... cargo test -p harn-orca --test live
//! -- --ignored`.

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use harn_core::{Policy, Profile};
use harn_orca::{Event, TerminalPin, WatchOptions, watch};

const GIVE_UP: Duration = Duration::from_secs(90);

#[test]
#[ignore = "needs a running Orca and a fake-agent terminal you created (HARN_ORCA_LIVE_HANDLE)"]
fn types_continue_into_a_pinned_terminal() {
    let handle = std::env::var("HARN_ORCA_LIVE_HANDLE").unwrap();
    let agent = "claude".to_owned();
    let pinned = vec![TerminalPin {
        handle: handle.clone(),
        agent,
    }];
    let opts = WatchOptions {
        interval_ms: 500,
        pinned,
        ..WatchOptions::default()
    };
    let mut policy = Policy {
        settle_ms: 1_000,
        ..Policy::default()
    };
    policy.reactions.overloaded.delay_secs = 1;
    let (stop, started) = (AtomicBool::new(false), Instant::now());
    let mut acted = false;
    let mut on_event = |event: &Event| {
        eprintln!("{event:?}");
        acted |= matches!(event, Event::Acted { dry_run: false, .. });
        if acted || started.elapsed() > GIVE_UP {
            stop.store(true, Ordering::Relaxed);
        }
    };
    watch(&opts, &Profile::builtins(), &policy, &mut on_event, &stop).unwrap();
    assert!(acted, "nothing typed within {GIVE_UP:?}");

    std::thread::sleep(Duration::from_secs(1));
    let terminal = format!("--terminal={handle}");
    let args = ["terminal", "read", &terminal, "--screen", "--json"];
    let out = Command::new("orca").args(args).output().unwrap();
    let screen = String::from_utf8_lossy(&out.stdout);
    assert!(screen.contains(r#"received: \"continue\""#), "{screen}");
    assert!(screen.contains(r#"received: \"\\r\""#), "{screen}");
}
