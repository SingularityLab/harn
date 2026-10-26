//! Usage-limit reset times from the profile's `reset_hints`: "try again in 2 hours 5 minutes",
//! "resets 3pm (Europe/Moscow)", "try again at 3:05 PM", "resets Oct 30, 9am", "…reached|1761490800".

use std::str::FromStr;

use jiff::Timestamp;
use jiff::civil::{Date, Time};
use jiff::tz::TimeZone;
use regex::Captures;

use crate::profile::Pattern;

/// A wall-clock reset time up to this far in the past is taken as just passed instead of being
/// pushed a day (or a year) ahead: a stale "resets 3pm" still on screen at 3:20pm must not park
/// the agent until tomorrow. The engine falls back to backoff for reset times in the past.
const PAST_TOLERANCE_MS: i64 = 6 * 3_600_000;

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// Relative-duration groups and their length in ms.
const UNITS: [(&str, u64); 4] = [
    ("days", 86_400_000),
    ("hours", 3_600_000),
    ("minutes", 60_000),
    ("seconds", 1_000),
];

const SECOND_MS: u64 = 1_000;

/// Real limits reset within a week or so; anything further out is a misparse or garbage (an
/// epoch in 2286), and trusting it would park the agent for months instead of backing off.
const MAX_AHEAD_MS: u64 = 35 * 86_400_000;

/// Epoch ms of the leftmost hint match in `text` that resolves to a plausible point in time.
pub(super) fn parse(hints: &[Pattern], text: &str, now_ms: u64, tz: &TimeZone) -> Option<u64> {
    let latest = now_ms.saturating_add(MAX_AHEAD_MS);
    hints
        .iter()
        .flat_map(|hint| hint.0.captures_iter(text))
        .filter_map(|caps| {
            let at = resolve(&caps, now_ms, tz).filter(|&at| at <= latest)?;
            Some((caps.get(0)?.start(), at))
        })
        .min_by_key(|(start, _)| *start)
        .map(|(_, at)| at)
}

fn resolve(caps: &Captures, now_ms: u64, tz: &TimeZone) -> Option<u64> {
    if caps.name("epoch").is_some() {
        return num::<u64>(caps, "epoch")?.checked_mul(SECOND_MS);
    }
    relative(caps, now_ms).or_else(|| wall_clock(caps, now_ms, tz))
}

fn relative(caps: &Captures, now_ms: u64) -> Option<u64> {
    let mut total: Option<u64> = None;
    for (group, unit_ms) in UNITS {
        if caps.name(group).is_some() {
            let n: u64 = num(caps, group)?;
            total = Some(total.unwrap_or(0).saturating_add(n.saturating_mul(unit_ms)));
        }
    }
    total.map(|ms| now_ms.saturating_add(ms))
}

fn wall_clock(caps: &Captures, now_ms: u64, tz: &TimeZone) -> Option<u64> {
    let minute: Option<i8> = num(caps, "minute");
    let ampm = caps.name("ampm").map(|m| m.as_str());
    // A bare number ("resets 3") is too ambiguous to be a time.
    if minute.is_none() && ampm.is_none() {
        return None;
    }
    let time = Time::new(to_24h(num(caps, "hour")?, ampm)?, minute.unwrap_or(0), 0, 0).ok()?;
    let tz = match caps.name("tz") {
        Some(name) => TimeZone::get(name.as_str()).ok()?,
        None => tz.clone(),
    };
    let now = i64::try_from(now_ms).ok()?;
    let today = Timestamp::from_millisecond(now)
        .ok()?
        .to_zoned(tz.clone())
        .date();
    candidate_dates(caps, today)?
        .into_iter()
        .filter_map(|date| date.to_datetime(time).to_zoned(tz.clone()).ok())
        .map(|zoned| zoned.timestamp().as_millisecond())
        .find(|&at| at >= now - PAST_TOLERANCE_MS)
        .and_then(|at| u64::try_from(at).ok())
}

/// Dates the time may fall on, ascending: the explicit date, or neighbours of today / this year.
fn candidate_dates(caps: &Captures, today: Date) -> Option<Vec<Date>> {
    let Some(month) = caps.name("month") else {
        return Some(vec![today.yesterday().ok()?, today, today.tomorrow().ok()?]);
    };
    let month = month_number(month.as_str())?;
    let day: i8 = num(caps, "day")?;
    let years = match num::<i16>(caps, "year") {
        Some(year) => vec![year],
        None => vec![today.year() - 1, today.year(), today.year() + 1],
    };
    Some(
        years
            .into_iter()
            .filter_map(|year| Date::new(year, month, day).ok())
            .collect(),
    )
}

fn to_24h(hour: i8, ampm: Option<&str>) -> Option<i8> {
    let Some(ampm) = ampm else {
        return (0..=23).contains(&hour).then_some(hour);
    };
    if !(1..=12).contains(&hour) {
        return None;
    }
    let pm = ampm.trim_start().starts_with(['p', 'P']);
    Some(hour % 12 + if pm { 12 } else { 0 })
}

fn month_number(name: &str) -> Option<i8> {
    let name = name.to_ascii_lowercase();
    let index = MONTHS.iter().position(|m| name.starts_with(m))?;
    i8::try_from(index + 1).ok()
}

fn num<T: FromStr>(caps: &Captures, group: &str) -> Option<T> {
    caps.name(group)?.as_str().parse().ok()
}

#[cfg(test)]
mod tests {
    use jiff::Zoned;

    use super::*;
    use crate::Profile;

    const HOUR: u64 = 3_600_000;
    const MINUTE: u64 = 60_000;

    fn ms(zoned: &str) -> u64 {
        let z: Zoned = zoned.parse().unwrap();
        u64::try_from(z.timestamp().as_millisecond()).unwrap()
    }

    fn msk() -> TimeZone {
        TimeZone::get("Europe/Moscow").unwrap()
    }

    /// Monday 2026-10-26 12:00 in Moscow (UTC+3, no DST).
    fn noon() -> u64 {
        ms("2026-10-26T12:00[Europe/Moscow]")
    }

    fn parse_at(text: &str, now_ms: u64) -> Option<u64> {
        let hints = &Profile::builtins()[0].signatures.reset_hints;
        parse(hints, text, now_ms, &msk())
    }

    #[test]
    fn relative_durations() {
        let now = noon();
        let cases = [
            ("try again in 2 hours 5 minutes.", 2 * HOUR + 5 * MINUTE),
            (
                "try again in 4 days 2 hours 35 minutes.",
                98 * HOUR + 35 * MINUTE,
            ),
            ("or try again in 45 minutes.", 45 * MINUTE),
            ("try again in 1 hour 23 minutes", HOUR + 23 * MINUTE),
            ("resets in 1 day", 24 * HOUR),
            ("resets in 2h 30m", 2 * HOUR + 30 * MINUTE),
            ("try again in 2 hours and 5 minutes", 2 * HOUR + 5 * MINUTE),
            ("try again in 30 seconds", 30_000),
        ];
        for (text, offset) in cases {
            assert_eq!(parse_at(text, now), Some(now + offset), "{text}");
        }
    }

    #[test]
    fn clock_times_resolve_to_today_in_injected_or_explicit_zone() {
        let now = noon();
        let cases = [
            (
                "Claude usage limit reached. Your limit will reset at 3pm (Europe/Moscow).",
                "2026-10-26T15:00[Europe/Moscow]",
            ),
            (
                "5-hour limit reached ∙ resets 3pm",
                "2026-10-26T15:00[Europe/Moscow]",
            ),
            ("resets 3:30pm", "2026-10-26T15:30[Europe/Moscow]"),
            (
                "You've hit your usage limit. Try again at 3:05 PM.",
                "2026-10-26T15:05[Europe/Moscow]",
            ),
            ("resets 15:00", "2026-10-26T15:00[Europe/Moscow]"),
            ("resets 12pm", "2026-10-26T12:00[Europe/Moscow]"),
            (
                "You've hit your limit · resets 3pm (America/New_York)",
                "2026-10-26T15:00[America/New_York]",
            ),
            ("resets 4pm (UTC)", "2026-10-26T16:00[UTC]"),
        ];
        for (text, expected) in cases {
            assert_eq!(parse_at(text, now), Some(ms(expected)), "{text}");
        }
    }

    #[test]
    fn clock_times_roll_over_unless_just_passed() {
        // 9 h ago is too old to be a stale message: it's tomorrow's reset.
        assert_eq!(
            parse_at("resets 3am", noon()),
            Some(ms("2026-10-27T03:00[Europe/Moscow]"))
        );
        // 3 h ago: the reset just passed (stale message), not tomorrow.
        assert_eq!(
            parse_at("resets 9am", noon()),
            Some(ms("2026-10-26T09:00[Europe/Moscow]"))
        );
        // Shortly after midnight, last night's time is still "just passed".
        let after_midnight = ms("2026-10-27T00:30[Europe/Moscow]");
        assert_eq!(
            parse_at("resets 11pm", after_midnight),
            Some(ms("2026-10-26T23:00[Europe/Moscow]"))
        );
        assert_eq!(
            parse_at("resets 2am", ms("2026-10-26T22:00[Europe/Moscow]")),
            Some(ms("2026-10-27T02:00[Europe/Moscow]"))
        );
    }

    #[test]
    fn dates() {
        let now = noon();
        let cases = [
            (
                "Weekly limit reached ∙ resets Oct 30, 9am",
                "2026-10-30T09:00[Europe/Moscow]",
            ),
            ("resets Oct 30 at 9:15am", "2026-10-30T09:15[Europe/Moscow]"),
            (
                "Try again at Oct 30th, 2026 9:15 AM.",
                "2026-10-30T09:15[Europe/Moscow]",
            ),
            (
                "resets Nov 2, 2026, 5:30pm",
                "2026-11-02T17:30[Europe/Moscow]",
            ),
            (
                "resets November 1, 10am (Europe/Moscow)",
                "2026-11-01T10:00[Europe/Moscow]",
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(parse_at(text, now), Some(ms(expected)), "{text}");
        }
        // Without a year, a date early next year resolves across the year boundary.
        let new_year_eve = ms("2026-12-30T12:00[Europe/Moscow]");
        assert_eq!(
            parse_at("resets Jan 2, 9am", new_year_eve),
            Some(ms("2027-01-02T09:00[Europe/Moscow]"))
        );
    }

    #[test]
    fn resets_more_than_35_days_ahead_are_unparsed() {
        for text in [
            // 68 days ahead (next January) or absurdly far.
            "resets Jan 2, 9am",
            "resets Nov 2, 2027, 5pm",
            "try again in 400 days",
            "Claude AI usage limit reached|9999999999",
        ] {
            assert_eq!(parse_at(text, noon()), None, "{text}");
        }
        assert_eq!(
            parse_at("try again in 34 days", noon()),
            Some(noon() + 34 * 24 * HOUR)
        );
    }

    #[test]
    fn epoch_seconds() {
        assert_eq!(
            parse_at("Claude AI usage limit reached|1761490800", noon()),
            Some(1_761_490_800_000)
        );
    }

    #[test]
    fn leftmost_match_wins() {
        let text = "Weekly limit reached ∙ resets Oct 30, 9am Tip: run it in 5 minutes";
        assert_eq!(
            parse_at(text, noon()),
            Some(ms("2026-10-30T09:00[Europe/Moscow]"))
        );
    }

    #[test]
    fn unparseable_is_none() {
        for text in [
            "Claude usage limit reached.",
            "try again later",
            "resets soon",
            "resets 3",
            "resets 25:00",
            "resets 13pm",
            "resets 3pm (Mars/Olympus_Mons)",
            "resets Feb 30, 9am",
            "Sign in to continue",
        ] {
            assert_eq!(parse_at(text, noon()), None, "{text}");
        }
    }
}
