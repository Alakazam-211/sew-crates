//! Wire times: [`Timestamp`] (a UTC instant), [`Date`] (a calendar day) and [`LocalDateTime`]
//! (a wall-clock minute with no zone).
//!
//! The calendar is the proleptic Gregorian one, without leap seconds (a `:60` second in input
//! counts as the first second of the next minute).

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::clock::{Clock, SystemClock};

const MS_PER_DAY: i64 = 86_400_000;

/// The earliest year a [`Timestamp`] can hold.
const TIMESTAMP_MIN_YEAR: i32 = -262_143;
/// The latest year a [`Timestamp`] can hold.
const TIMESTAMP_MAX_YEAR: i32 = 262_142;

/// Days from 1970-01-01 to `year-month-day` (proleptic Gregorian).
const fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = if month <= 2 {
        year as i64 - 1
    } else {
        year as i64
    };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The `(year, month, day)` of `days` after 1970-01-01 (proleptic Gregorian).
const fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

const fn is_leap_year(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

const fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Writes a year as RFC 3339 does: four digits for 0–9999, else signed.
fn write_year(f: &mut impl fmt::Write, year: i64) -> fmt::Result {
    if (0..=9999).contains(&year) {
        write!(f, "{year:04}")
    } else {
        write!(f, "{year:+05}")
    }
}

/// Exactly `n` ASCII digits at `b[at..at + n]`, as a number.
fn digits(b: &[u8], at: usize, n: usize) -> Option<u32> {
    let s = b.get(at..at + n)?;
    s.iter().try_fold(0u32, |acc, &c| {
        c.is_ascii_digit().then(|| acc * 10 + u32::from(c - b'0'))
    })
}

/// A UTC instant with millisecond precision.
///
/// JSON: an RFC 3339 string, always UTC with three fraction digits and a `Z` suffix, for example
/// `"2026-09-27T12:00:00.123Z"`. Input accepts any RFC 3339 string with an offset; it is
/// converted to UTC and truncated to milliseconds.
///
/// The range is the years -262143 to 262142 ([`Timestamp::MIN`] to [`Timestamp::MAX`]).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);

impl Timestamp {
    /// The earliest timestamp, `-262143-01-01T00:00:00.000Z`.
    pub const MIN: Timestamp = Timestamp(days_from_civil(TIMESTAMP_MIN_YEAR, 1, 1) * MS_PER_DAY);
    /// The latest timestamp, `+262142-12-31T23:59:59.999Z`.
    pub const MAX: Timestamp =
        Timestamp((days_from_civil(TIMESTAMP_MAX_YEAR, 12, 31) + 1) * MS_PER_DAY - 1);

    /// The current time from the system clock. Server code uses its injected [`Clock`] instead.
    pub fn now() -> Self {
        SystemClock.now()
    }

    /// Builds a timestamp from Unix milliseconds. `None` outside [`Timestamp::MIN`] to
    /// [`Timestamp::MAX`].
    pub const fn from_unix_ms(ms: i64) -> Option<Self> {
        if ms < Self::MIN.0 || ms > Self::MAX.0 {
            None
        } else {
            Some(Timestamp(ms))
        }
    }

    /// Builds a timestamp from unsigned Unix milliseconds, clamping values past the range to
    /// [`Timestamp::MAX`]. Used by clocks, which never read beyond year 262142.
    pub const fn from_unix_ms_saturating(ms: u64) -> Self {
        if ms > Self::MAX.0 as u64 {
            Self::MAX
        } else {
            Timestamp(ms as i64)
        }
    }

    /// Milliseconds since the Unix epoch.
    pub const fn unix_ms(self) -> i64 {
        self.0
    }

    /// The UTC calendar day, as `(year, month, day)`.
    fn ymd(self) -> (i64, u32, u32) {
        civil_from_days(self.0.div_euclid(MS_PER_DAY))
    }

    /// The wire string: RFC 3339, UTC, milliseconds, `Z`.
    pub fn to_wire(self) -> String {
        self.to_string()
    }

    /// Parses RFC 3339 (`YYYY-MM-DDTHH:MM:SS[.fraction](Z|±HH:MM)`, `T` or a space between date
    /// and time, any number of fraction digits) into UTC milliseconds, truncating.
    fn parse_rfc3339(s: &str) -> Result<i64, &'static str> {
        let b = s.as_bytes();
        let num = |at: usize, n: usize| digits(b, at, n).ok_or("expected a digit");
        let sep = |at: usize, c: u8| match b.get(at) {
            Some(&x) if x == c => Ok(()),
            Some(_) => Err("unexpected character"),
            None => Err("too short"),
        };
        let year = num(0, 4)?;
        sep(4, b'-')?;
        let month = num(5, 2)?;
        sep(7, b'-')?;
        let day = num(8, 2)?;
        match b.get(10) {
            Some(b'T' | b't' | b' ') => {}
            Some(_) => return Err("unexpected character"),
            None => return Err("too short"),
        }
        let hour = num(11, 2)?;
        sep(13, b':')?;
        let minute = num(14, 2)?;
        sep(16, b':')?;
        let second = num(17, 2)?;
        let mut at = 19;
        let mut nanos: i64 = 0;
        if b.get(at) == Some(&b'.') {
            at += 1;
            let start = at;
            while b.get(at).is_some_and(u8::is_ascii_digit) {
                if at - start < 9 {
                    nanos = nanos * 10 + i64::from(b[at] - b'0');
                }
                at += 1;
            }
            let n = at - start;
            if n == 0 {
                return Err("expected a digit");
            }
            for _ in n..9 {
                nanos *= 10;
            }
        }
        let rest = &s[at..];
        let offset_s: i64 = match rest.as_bytes().first() {
            Some(b'Z' | b'z') if rest.len() == 1 => 0,
            Some(b'Z' | b'z') => return Err("trailing input"),
            None => return Err("too short"),
            Some(_) => {
                let (negative, tail) = if let Some(t) = rest.strip_prefix('+') {
                    (false, t)
                } else if let Some(t) = rest.strip_prefix('-') {
                    (true, t)
                } else if let Some(t) = rest.strip_prefix('\u{2212}') {
                    (true, t)
                } else {
                    return Err("unexpected character");
                };
                let t = tail.as_bytes();
                let oh = digits(t, 0, 2).ok_or("bad offset")?;
                if t.get(2) != Some(&b':') {
                    return Err("bad offset");
                }
                let om = digits(t, 3, 2).ok_or("bad offset")?;
                if t.len() != 5 {
                    return Err("trailing input");
                }
                if om > 59 || oh > 23 {
                    return Err("offset out of range");
                }
                let secs = i64::from(oh * 3600 + om * 60);
                if negative { -secs } else { secs }
            }
        };
        let year = i32::try_from(year).map_err(|_| "out of range")?;
        if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
            return Err("no such date");
        }
        if hour > 23 || minute > 59 || second > 60 {
            return Err("no such time");
        }
        // A leap second is the 59th second plus one: the next minute's first second.
        let secs = i64::from(hour * 3600 + minute * 60 + second);
        let ms = days_from_civil(year, month, day) * MS_PER_DAY
            + (secs - offset_s) * 1000
            + nanos / 1_000_000;
        Ok(ms)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (year, month, day) = self.ymd();
        let in_day = self.0.rem_euclid(MS_PER_DAY);
        let (h, m) = (in_day / 3_600_000, in_day / 60_000 % 60);
        let (s, ms) = (in_day / 1000 % 60, in_day % 1000);
        write_year(f, year)?;
        write!(f, "-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.{ms:03}Z")
    }
}

impl fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Timestamp({self})")
    }
}

/// Error parsing a [`Timestamp`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimestampError(String);

impl fmt::Display for TimestampError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid RFC 3339 timestamp: {}", self.0)
    }
}

impl std::error::Error for TimestampError {}

impl FromStr for Timestamp {
    type Err = TimestampError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Timestamp::parse_rfc3339(s)
            .and_then(|ms| Timestamp::from_unix_ms(ms).ok_or("out of range"))
            .map_err(|e| TimestampError(format!("{s:?}: {e}")))
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = Cow::<'de, str>::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Timestamp {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        "Timestamp".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "format": "date-time",
            "description": "RFC 3339 UTC time with milliseconds, e.g. 2026-09-27T12:00:00.123Z."
        })
    }
}

/// A calendar day with a four-digit year (0000–9999), with no time zone: `YYYY-MM-DD`.
///
/// Ordered by date.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    year: u16,
    month: u8,
    day: u8,
}

impl Date {
    /// The day `year-month-day`. `None` for a year outside 0–9999 or a day the month does not
    /// have.
    pub const fn from_ymd(year: i32, month: u32, day: u32) -> Option<Date> {
        if year < 0 || year > 9999 || month < 1 || month > 12 {
            return None;
        }
        if day < 1 || day > days_in_month(year, month) {
            return None;
        }
        Some(Date {
            year: year as u16,
            month: month as u8,
            day: day as u8,
        })
    }

    /// Parses a strict `YYYY-MM-DD`.
    pub fn parse(s: &str) -> Option<Date> {
        let b = s.as_bytes();
        if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
            return None;
        }
        Date::from_ymd(
            i32::try_from(digits(b, 0, 4)?).ok()?,
            digits(b, 5, 2)?,
            digits(b, 8, 2)?,
        )
    }

    /// The year.
    pub const fn year(self) -> i32 {
        self.year as i32
    }

    /// The month, 1–12.
    pub const fn month(self) -> u32 {
        self.month as u32
    }

    /// The day of the month, from 1.
    pub const fn day(self) -> u32 {
        self.day as u32
    }

    /// The next day; `None` after 9999-12-31.
    pub const fn succ(self) -> Option<Date> {
        let (y, m, d) = (self.year(), self.month(), self.day());
        if d < days_in_month(y, m) {
            Date::from_ymd(y, m, d + 1)
        } else if m < 12 {
            Date::from_ymd(y, m + 1, 1)
        } else {
            Date::from_ymd(y + 1, 1, 1)
        }
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

impl fmt::Debug for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// A wall-clock minute with no time zone: `YYYY-MM-DDTHH:MM` (no seconds, no offset).
///
/// Ordered by date, then time.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalDateTime {
    date: Date,
    hour: u8,
    minute: u8,
}

impl LocalDateTime {
    /// `date` at `hour:minute`. `None` for an hour past 23 or a minute past 59.
    pub const fn new(date: Date, hour: u32, minute: u32) -> Option<LocalDateTime> {
        if hour > 23 || minute > 59 {
            return None;
        }
        Some(LocalDateTime {
            date,
            hour: hour as u8,
            minute: minute as u8,
        })
    }

    /// Parses a strict `YYYY-MM-DDTHH:MM`.
    pub fn parse(s: &str) -> Option<LocalDateTime> {
        let b = s.as_bytes();
        if b.len() != 16 || b[10] != b'T' || b[13] != b':' {
            return None;
        }
        LocalDateTime::new(Date::parse(&s[..10])?, digits(b, 11, 2)?, digits(b, 14, 2)?)
    }

    /// The day.
    pub const fn date(self) -> Date {
        self.date
    }

    /// The hour, 0–23.
    pub const fn hour(self) -> u32 {
        self.hour as u32
    }

    /// The minute, 0–59.
    pub const fn minute(self) -> u32 {
        self.minute as u32
    }
}

impl fmt::Display for LocalDateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}T{:02}:{:02}", self.date, self.hour, self.minute)
    }
}

impl fmt::Debug for LocalDateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn civil_round_trips() {
        for days in (-200_000i64..200_000)
            .step_by(7)
            .chain([-719_468, 0, 1, 59, 60])
        {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y as i32, m, d), days, "{y}-{m}-{d}");
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    }

    #[test]
    fn range_edges() {
        assert_eq!(Timestamp::MIN.to_wire(), "-262143-01-01T00:00:00.000Z");
        assert_eq!(Timestamp::MAX.to_wire(), "+262142-12-31T23:59:59.999Z");
        assert!(Timestamp::from_unix_ms(Timestamp::MIN.unix_ms() - 1).is_none());
        assert!(Timestamp::from_unix_ms(Timestamp::MAX.unix_ms() + 1).is_none());
        assert_eq!(Timestamp::from_unix_ms_saturating(u64::MAX), Timestamp::MAX);
    }
}
