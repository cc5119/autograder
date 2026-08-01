//! Human-facing formatting shared by the commands that print records --
//! `fetch`'s confirmation manifest and `show`. Machine-readable output
//! (the JSON records, `report`) keeps its own timestamps untouched.

use jiff::{Timestamp, Zoned};

/// "Sat 14 Feb 2026, 23:59 America/Santiago" -- the IANA zone rather than
/// the offset, since that's the field an instructor would recognize as
/// wrong.
pub fn datetime(when: &Zoned) -> String {
    when.strftime("%a %-d %b %Y, %H:%M %Q").to_string()
}

/// The same, for a UTC timestamp shown in the machine's own zone.
pub fn instant(when: Timestamp) -> String {
    datetime(&when.to_zoned(jiff::tz::TimeZone::system()))
}

/// "2 days ago" / "in 6 hours", bare: callers decide the punctuation and
/// whether being in the future deserves a warning.
pub fn relative(when: &Zoned, now: &Zoned) -> String {
    let seconds = when.timestamp().as_second() - now.timestamp().as_second();
    let amount = duration_secs(seconds.unsigned_abs());
    if seconds <= 0 {
        format!("{amount} ago")
    } else {
        format!("in {amount}")
    }
}

/// An elapsed time on its own: "3 hours", "2 days". Sign is the caller's
/// to phrase.
pub fn duration(elapsed: jiff::SignedDuration) -> String {
    duration_secs(elapsed.as_secs().unsigned_abs())
}

/// One coarse unit, not "2 days 3 hours 12 minutes" -- these read at a
/// glance, and the exact instant is always on the line next to them.
fn duration_secs(seconds: u64) -> String {
    match seconds {
        0..=90 => "moments".to_string(),
        s if s < 3600 => plural(s / 60, "minute"),
        s if s < 86_400 => plural(s / 3600, "hour"),
        s => plural(s / 86_400, "day"),
    }
}

fn plural(n: u64, unit: &str) -> String {
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}
