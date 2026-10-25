//! XBEL timestamps: the ISO 8601 subset `GLib` writes
//! (`2023-05-04T10:17:17.138078Z`), parsed to and formatted from
//! [`SystemTime`] without a calendar dependency. The civil-date arithmetic
//! is the well-known era/day-of-era formulation (as in C++20's
//! `<chrono>`), exact over the whole range used here.

use std::time::{Duration, SystemTime};

use crate::work;

/// Seconds in a day.
const DAY: i64 = 86_400;

/// Parses `YYYY-MM-DDThh:mm:ss`, an optional fraction of 1–9 digits, and
/// a zone of `Z` or `±hh:mm`. Anything else — an invalid date included —
/// is `None`.
pub(crate) fn parse(text: &str) -> Option<SystemTime> {
    // Each byte of the text examined.
    work::count(text.len());
    let bytes = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| -> Option<i64> {
        let mut value = 0i64;
        for &byte in bytes.get(range)? {
            if !byte.is_ascii_digit() {
                return None;
            }
            value = value * 10 + i64::from(byte - b'0');
        }
        Some(value)
    };
    let expect = |at: usize, what: u8| (bytes.get(at) == Some(&what)).then_some(());
    let year = digits(0..4)?;
    expect(4, b'-')?;
    let month = digits(5..7)?;
    expect(7, b'-')?;
    let day = digits(8..10)?;
    expect(10, b'T')?;
    let hour = digits(11..13)?;
    expect(13, b':')?;
    let minute = digits(14..16)?;
    expect(16, b':')?;
    let second = digits(17..19)?;
    if !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let mut at = 19;
    let mut nanos: u32 = 0;
    if bytes.get(at) == Some(&b'.') {
        let digits_start = at + 1;
        let mut scale = 100_000_000u32;
        at = digits_start;
        while let Some(byte) = bytes.get(at).filter(|byte| byte.is_ascii_digit()) {
            if at - digits_start == 9 {
                return None;
            }
            nanos += u32::from(byte - b'0') * scale;
            scale /= 10;
            at += 1;
        }
        if at == digits_start {
            return None;
        }
    }
    let offset = match bytes.get(at) {
        Some(b'Z') if at + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-')) if at + 6 == bytes.len() => {
            expect(at + 3, b':')?;
            let hours = digits(at + 1..at + 3)?;
            let minutes = digits(at + 4..at + 6)?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            let offset = hours * 3600 + minutes * 60;
            if *sign == b'-' { -offset } else { offset }
        }
        _ => return None,
    };
    let seconds =
        days_from_civil(year, month, day) * DAY + hour * 3600 + minute * 60 + second - offset;
    Some(if seconds >= 0 {
        SystemTime::UNIX_EPOCH + Duration::new(seconds.unsigned_abs(), nanos)
    } else if nanos == 0 {
        SystemTime::UNIX_EPOCH - Duration::new(seconds.unsigned_abs(), 0)
    } else {
        // Nanoseconds always run forward: borrow one second.
        SystemTime::UNIX_EPOCH - Duration::new(seconds.unsigned_abs(), 0) + Duration::new(0, nanos)
    })
}

/// Formats UTC with up to nine fractional digits, trailing zeros trimmed
/// and the whole fraction omitted when zero, so everything [`parse`]
/// accepts round-trips. Covers the years 1583–9999; outside them there is
/// no `None`-free XBEL spelling, so the caller refuses the time.
pub(crate) fn format(time: SystemTime) -> Option<String> {
    let (seconds, nanos) = match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(since) => (i64::try_from(since.as_secs()).ok()?, since.subsec_nanos()),
        Err(earlier) => {
            let before = earlier.duration();
            let mut seconds = -i64::try_from(before.as_secs()).ok()?;
            let mut nanos = before.subsec_nanos();
            if nanos != 0 {
                // Nanoseconds always run forward: borrow one second.
                seconds -= 1;
                nanos = 1_000_000_000 - nanos;
            }
            (seconds, nanos)
        }
    };
    let days = seconds.div_euclid(DAY);
    let of_day = seconds.rem_euclid(DAY);
    let (year, month, day) = civil_from_days(days);
    if !(1583..=9999).contains(&year) {
        return None;
    }
    let mut written = format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60
    );
    if nanos != 0 {
        let mut fraction = format!(".{nanos:09}");
        while fraction.ends_with('0') {
            fraction.pop();
        }
        written.push_str(&fraction);
    }
    written.push('Z');
    Some(written)
}

/// Days from 1970-01-01 to `year`-`month`-`day` (proleptic Gregorian).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let of_era = year - era * 400; // [0, 399]
    let of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = of_era * 365 + of_era / 4 - of_era / 100 + of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The civil date `days` after 1970-01-01 (proleptic Gregorian).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097; // [0, 146096]
    let of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = of_era + era * 400;
    let of_year = day_of_era - (365 * of_era + of_era / 4 - of_era / 100);
    let shifted_month = (5 * of_year + 2) / 153; // [0, 11]
    let day = of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    (year + i64::from(month <= 2), month, day)
}

/// How many days `month` has in `year`.
fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        _ => 28,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(text: &str) {
        let time = parse(text).unwrap();
        assert_eq!(format(time).unwrap(), text, "{text}");
    }

    #[test]
    fn glib_timestamps_round_trip() {
        round_trip("2023-05-04T10:17:17.138078Z");
        round_trip("2002-01-23T14:53:21Z");
        round_trip("2024-02-29T23:59:59.000000001Z");
        round_trip("1969-12-31T23:59:59Z");
        round_trip("1969-12-31T23:59:59.5Z");
        round_trip("1583-01-01T00:00:00Z");
        round_trip("9999-12-31T23:59:59.999999999Z");
    }

    #[test]
    fn the_epoch_and_known_dates_are_exact() {
        assert_eq!(
            parse("1970-01-01T00:00:00Z").unwrap(),
            SystemTime::UNIX_EPOCH
        );
        assert_eq!(
            parse("2001-09-09T01:46:40Z").unwrap(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000)
        );
    }

    #[test]
    fn an_offset_shifts_to_utc() {
        assert_eq!(
            parse("2023-05-04T12:17:17+02:00").unwrap(),
            parse("2023-05-04T10:17:17Z").unwrap()
        );
        assert_eq!(
            parse("2023-05-04T08:47:17-01:30").unwrap(),
            parse("2023-05-04T10:17:17Z").unwrap()
        );
    }

    #[test]
    fn the_rest_is_rejected() {
        for bad in [
            "",
            "2023-05-04",                      // no time
            "2023-05-04T10:17:17",             // no zone
            "2023-13-04T10:17:17Z",            // month
            "2023-02-29T10:17:17Z",            // not a leap year
            "1900-02-29T10:17:17Z",            // centuries are not either
            "2023-05-04T24:00:00Z",            // hour
            "2023-05-04T10:60:17Z",            // minute
            "2023-05-04T10:17:60Z",            // leap seconds are not written
            "2023-05-04T10:17:17.Z",           // empty fraction
            "2023-05-04T10:17:17.0123456789Z", // ten digits
            "2023-05-04T10:17:17+2:00",        // short offset hour
            "2023-05-04T10:17:17+02:60",       // offset minute
            "2023-05-04T10:17:17Zz",           // trailing junk
            "2023-05-04 10:17:17Z",            // space, not T
            "+023-05-04T10:17:17Z",            // sign in the year
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn leap_day_parses() {
        round_trip("2000-02-29T12:00:00Z");
        round_trip("2024-02-29T12:00:00Z");
    }

    #[test]
    fn out_of_range_years_cannot_be_formatted() {
        let early = parse("1583-01-01T00:00:00Z").unwrap() - Duration::from_secs(1);
        assert_eq!(format(early), None);
        let late = parse("9999-12-31T23:59:59Z").unwrap() + Duration::from_secs(1);
        assert_eq!(format(late), None);
    }

    #[test]
    fn every_day_of_a_leap_cycle_round_trips() {
        // Four years crossing a leap day and a century rule.
        let mut time = parse("1999-01-01T00:00:00Z").unwrap();
        let end = parse("2003-01-01T00:00:00Z").unwrap();
        while time < end {
            let text = format(time).unwrap();
            assert_eq!(parse(&text), Some(time), "{text}");
            time += Duration::from_secs(u64::try_from(DAY).unwrap());
        }
    }
}
