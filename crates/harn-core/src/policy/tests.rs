use super::*;

fn parse(src: &str) -> Result<Policy, toml::de::Error> {
    toml::from_str(src)
}

#[test]
fn defaults_match_spec_and_validate() {
    let p = Policy::default();
    assert_eq!(p.validate(), Ok(()));
    assert_eq!(parse("").unwrap(), p);
    let limits = (
        p.settle_ms,
        p.effect_timeout_ms,
        p.recovery_ms,
        p.stall_after_ms,
    );
    assert_eq!(limits, (4_000, 90_000, 120_000, 900_000));
    assert_eq!((p.user_grace_ms, p.max_actions_per_hour), (30_000, 40));
    use ReactionKind::{Continue, Ignore, Notify};
    let shape = |t| {
        let r = p.reactions.get(t);
        (r.action, r.delay_secs, r.max_delay_secs, r.max_attempts)
    };
    assert_eq!(shape(Trigger::Overloaded), (Continue, 20, 600, 30));
    assert_eq!(shape(Trigger::ServerError), (Continue, 20, 600, 20));
    assert_eq!(shape(Trigger::Network), (Continue, 10, 300, 30));
    assert_eq!(shape(Trigger::RateLimited), (Continue, 60, 900, 20));
    assert_eq!(shape(Trigger::UsageLimit), (Continue, 60, 1800, 50));
    assert_eq!(p.reactions.usage_limit.fallback_delay_secs, Some(900));
    assert_eq!(p.reactions.get(Trigger::Stalled).action, Notify);
    assert_eq!(p.reactions.get(Trigger::StoppedEarly).action, Ignore);
}

#[test]
fn partial_tables_keep_that_triggers_defaults() {
    let src = "settle_ms = 1000\n[reactions.overloaded]\naction = \"notify\"\n\
               [reactions.usage_limit]\ntext = \"go on\"\n";
    let (p, d) = (parse(src).unwrap(), Policy::default().reactions);
    assert_eq!(p.settle_ms, 1000);
    let notify = ReactionKind::Notify;
    let overloaded = d.overloaded.clone();
    assert_eq!(
        p.reactions.overloaded,
        Reaction {
            action: notify,
            ..overloaded
        }
    );
    let usage = d.usage_limit.clone();
    let text = Some("go on".to_owned());
    assert_eq!(p.reactions.usage_limit, Reaction { text, ..usage });
    assert_eq!(p.reactions.server_error, d.server_error);
    for bad in [
        "[reactions.overloaded]\nactoin = \"notify\"",
        "[reactions.overlaoded]\naction = \"notify\"",
        "[reactions.overloaded]\naction = \"retry\"",
        "settle = 1",
    ] {
        assert!(parse(bad).is_err(), "{bad}");
    }
}

/// `config => field value min`: a config breaking one floor, and the error it must produce.
const FLOORS: &str = "
settle_ms = 499 => settle_ms 499 500
effect_timeout_ms = 3999 => effect_timeout_ms 3999 4000
max_actions_per_hour = 0 => max_actions_per_hour 0 1
[reactions.overloaded] delay_secs = 0 => reactions.overloaded.delay_secs 0 1
[reactions.network] max_delay_secs = 5 => reactions.network.max_delay_secs 5 10
[reactions.usage_limit] max_attempts = 0 => reactions.usage_limit.max_attempts 0 1
[reactions.usage_limit] fallback_delay_secs = 0 => reactions.usage_limit.fallback_delay_secs 0 1
";

#[test]
fn validate_enforces_floors() {
    for row in FLOORS.lines().filter(|row| !row.is_empty()) {
        let (config, error) = row.split_once(" => ").unwrap();
        let p = parse(&config.replace("] ", "]\n")).unwrap();
        let [field, value, min] = error.split(' ').collect::<Vec<_>>().try_into().unwrap();
        let (value, min) = (value.parse().unwrap(), min.parse().unwrap());
        let expected = PolicyError::TooSmall {
            field: field.into(),
            value,
            min,
        };
        assert_eq!(p.validate(), Err(expected), "{row}");
    }
    // Reactions that never type are not checked; a stall fires once, so its delays are unused.
    let src = "[reactions.overloaded]\naction = \"notify\"\ndelay_secs = 0\n\
               [reactions.stalled]\naction = \"interrupt\"\n";
    assert_eq!(parse(src).unwrap().validate(), Ok(()));
    let message = parse("settle_ms = 100")
        .unwrap()
        .validate()
        .unwrap_err()
        .to_string();
    assert_eq!(
        message,
        "policy: settle_ms = 100 is below the minimum of 500"
    );
}
