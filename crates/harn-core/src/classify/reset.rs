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
mod tests;
