use jiff::Zoned;

use super::*;
use crate::Profile;

/// `[@now |] text => reset`: civil times are Moscow's (UTC+3, no DST) unless a zone is given,
/// `now` defaults to Monday 2026-10-26 12:00, and `-` means unparsed (the engine backs off).
const CASES: &str = "
try again in 2 hours 5 minutes. => 2026-10-26T14:05
try again in 4 days 2 hours 35 minutes. => 2026-10-30T14:35
resets in 2h 30m => 2026-10-26T14:30
try again in 2 hours and 5 minutes => 2026-10-26T14:05
try again in 30 seconds => 2026-10-26T12:00:30
Your limit will reset at 3pm (Europe/Moscow). => 2026-10-26T15:00
● You've hit your session limit · resets 11:47pm (Europe/Moscow) => 2026-10-26T23:47
resets 3:30pm => 2026-10-26T15:30
Try again at 3:05 PM. => 2026-10-26T15:05
resets 15:00 => 2026-10-26T15:00
resets 12pm => 2026-10-26T12:00
resets 3pm (America/New_York) => 2026-10-26T15:00[America/New_York]
resets 4pm (UTC) => 2026-10-26T16:00[UTC]
# A wall-clock time 9 h ago is tomorrow's; 3 h ago it is a reset that just passed.
resets 3am => 2026-10-27T03:00
resets 9am => 2026-10-26T09:00
@2026-10-27T00:30 | resets 11pm => 2026-10-26T23:00
@2026-10-26T22:00 | resets 2am => 2026-10-27T02:00
Weekly limit reached ∙ resets Oct 30, 9am => 2026-10-30T09:00
resets Oct 30 at 9:15am => 2026-10-30T09:15
Try again at Oct 30th, 2026 9:15 AM. => 2026-10-30T09:15
resets Nov 2, 2026, 5:30pm => 2026-11-02T17:30
resets November 1, 10am (Europe/Moscow) => 2026-11-01T10:00
@2026-12-30T12:00 | resets Jan 2, 9am => 2027-01-02T09:00
Claude AI usage limit reached|1761490800 => 2025-10-26T18:00
resets Oct 30, 9am Tip: run it in 5 minutes => 2026-10-30T09:00
# More than 35 days ahead is a misparse or garbage.
try again in 34 days => 2026-11-29T12:00
resets Jan 2, 9am => -
resets Nov 2, 2027, 5pm => -
try again in 400 days => -
Claude AI usage limit reached|9999999999 => -
Claude usage limit reached. => -
try again later => -
resets 3 => -
resets 25:00 => -
resets 13pm => -
resets 3pm (Mars/Olympus_Mons) => -
resets Feb 30, 9am => -
Sign in to continue => -
";

fn ms(civil: &str) -> u64 {
    let zoned = if civil.contains('[') {
        civil.to_owned()
    } else {
        format!("{civil}[Europe/Moscow]")
    };
    let z: Zoned = zoned.parse().unwrap();
    u64::try_from(z.timestamp().as_millisecond()).unwrap()
}

#[test]
fn reset_times() {
    let hints = &Profile::builtins()[0].signatures.reset_hints;
    let tz = TimeZone::get("Europe/Moscow").unwrap();
    for row in CASES
        .lines()
        .filter(|row| !row.is_empty() && !row.starts_with('#'))
    {
        let (case, expected) = row.rsplit_once(" => ").unwrap();
        let (now, text) = match case.strip_prefix('@') {
            Some(rest) => rest.split_once(" | ").unwrap(),
            None => ("2026-10-26T12:00", case),
        };
        let expected = (expected != "-").then(|| ms(expected));
        assert_eq!(parse(hints, text, ms(now), &tz), expected, "{row}");
    }
}
