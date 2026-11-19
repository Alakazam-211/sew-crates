//! `google.protobuf.Timestamp`, the type of the generated timestamp fields.

use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// `google.protobuf.Timestamp`: an instant as seconds and nanoseconds since the Unix epoch.
///
/// Binary: the protobuf message (`seconds` is field 1, `nanos` field 2).
///
/// JSON: an RFC 3339 string in UTC with a `+00:00` offset and as many fraction digits as the
/// value needs (none, 3, 6 or 9), for example `"2023-11-08T16:07:09.250+00:00"`. Input accepts any
/// RFC 3339 string (`T`, `t` or a space between date and time; `Z`, `z` or an offset up to
/// ±23:59). Serializing fails for `nanos` outside 0–999,999,999 (1,999,999,999 on a leap second,
/// whose `seconds` ends a minute) and for an instant outside the years -262143 to 262142.
#[derive(Clone, Copy, PartialEq, Eq, Hash, ::prost::Message)]
pub struct Timestamp {
    /// Seconds since 1970-01-01T00:00:00Z.
    #[prost(int64, tag = "1")]
    pub seconds: i64,
    /// Nanoseconds past `seconds`, counting forward in time.
    #[prost(int32, tag = "2")]
    pub nanos: i32,
}

const SECS_PER_DAY: i64 = 86_400;
const MIN_YEAR: i64 = -262_143;
const MAX_YEAR: i64 = 262_142;

/// Days from 1970-01-01 to `year-month-day` (proleptic Gregorian).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(month) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The `(year, month, day)` of `days` after 1970-01-01 (proleptic Gregorian).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    // `doy` is 0..=365 and `mp` 0..=11, so both fit.
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    }
}

/// Exactly `n` ASCII digits at `b[at..at + n]`, as a number.
fn digits(b: &[u8], at: usize, n: usize) -> Option<u32> {
    let s = b.get(at..at + n)?;
    s.iter().try_fold(0u32, |acc, &c| {
        c.is_ascii_digit().then(|| acc * 10 + u32::from(c - b'0'))
    })
}

impl Timestamp {
    /// The JSON string, or why the value has none.
    fn to_rfc3339(self) -> Result<String, &'static str> {
        let nanos = u32::try_from(self.nanos)
            .map_err(|_| "out of range integral type conversion attempted")?;
        const INVALID: &str = "invalid or out-of-range datetime";
        let days = self.seconds.div_euclid(SECS_PER_DAY);
        let in_day = self.seconds.rem_euclid(SECS_PER_DAY);
        let (year, month, day) = civil_from_days(days);
        if !(MIN_YEAR..=MAX_YEAR).contains(&year) {
            return Err(INVALID);
        }
        // A leap second is `:59` with `nanos` of one second or more.
        let leap = nanos >= 1_000_000_000;
        if nanos >= 2_000_000_000 || (leap && in_day % 60 != 59) {
            return Err(INVALID);
        }
        let (mut sec, nano) = (in_day % 60, nanos % 1_000_000_000);
        if leap {
            sec += 1;
        }
        let mut out = String::with_capacity(32);
        if (0..=9999).contains(&year) {
            out.push_str(&format!("{year:04}"));
        } else {
            out.push_str(&format!("{year:+05}"));
        }
        out.push_str(&format!(
            "-{month:02}-{day:02}T{:02}:{:02}:{sec:02}",
            in_day / 3600,
            in_day / 60 % 60
        ));
        // As few fraction digits as the value needs: none, 3, 6 or 9.
        match nano {
            0 => {}
            n if n % 1_000_000 == 0 => out.push_str(&format!(".{:03}", n / 1_000_000)),
            n if n % 1_000 == 0 => out.push_str(&format!(".{:06}", n / 1_000)),
            n => out.push_str(&format!(".{n:09}")),
        }
        out.push_str("+00:00");
        Ok(out)
    }

    /// Parses RFC 3339 (`YYYY-MM-DD(T|t| )HH:MM:SS[.fraction](Z|z|±HH:MM)`; fraction digits
    /// past the ninth are ignored).
    fn parse_rfc3339(s: &str) -> Result<Timestamp, &'static str> {
        let b = s.as_bytes();
        let num = |at: usize, n: usize| digits(b, at, n).ok_or("expected a digit");
        let sep = |at: usize, c: u8| match b.get(at) {
            Some(&x) if x == c => Ok(()),
            Some(_) => Err("unexpected character"),
            None => Err("premature end of input"),
        };
        let year = i64::from(num(0, 4)?);
        sep(4, b'-')?;
        let month = num(5, 2)?;
        sep(7, b'-')?;
        let day = num(8, 2)?;
        match b.get(10) {
            Some(b'T' | b't' | b' ') => {}
            Some(_) => return Err("unexpected character"),
            None => return Err("premature end of input"),
        }
        let hour = num(11, 2)?;
        sep(13, b':')?;
        let minute = num(14, 2)?;
        sep(16, b':')?;
        let second = num(17, 2)?;
        let mut at = 19;
        let mut nano: u32 = 0;
        if b.get(at) == Some(&b'.') {
            at += 1;
            let start = at;
            while b.get(at).is_some_and(u8::is_ascii_digit) {
                if at - start < 9 {
                    nano = nano * 10 + u32::from(b[at] - b'0');
                }
                at += 1;
            }
            let n = at - start;
            if n == 0 {
                return Err("expected a digit");
            }
            for _ in n..9 {
                nano *= 10;
            }
        }
        let rest = &s[at..];
        let offset: i64 = match rest.as_bytes().first() {
            Some(b'Z' | b'z') if rest.len() == 1 => 0,
            Some(b'Z' | b'z') => return Err("trailing input"),
            None => return Err("premature end of input"),
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
        if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
            return Err("no such date");
        }
        if hour > 23 || minute > 59 || second > 60 {
            return Err("no such time");
        }
        // A `:60` leap second is kept as `:59` plus one second of `nanos`.
        let (second, nano) = if second == 60 {
            (59, nano + 1_000_000_000)
        } else {
            (second, nano)
        };
        let secs = i64::from(hour * 3600 + minute * 60 + second);
        let seconds = days_from_civil(year, month, day) * SECS_PER_DAY + secs - offset;
        let nanos = i32::try_from(nano).map_err(|_| "out of range")?;
        Ok(Timestamp { seconds, nanos })
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let s = self.to_rfc3339().map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&s)
    }
}

struct TimestampVisitor;

impl Visitor<'_> for TimestampVisitor {
    type Value = Timestamp;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a date string")
    }

    fn visit_str<E: de::Error>(self, s: &str) -> Result<Timestamp, E> {
        Timestamp::parse_rfc3339(s).map_err(E::custom)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(TimestampVisitor)
    }
}
