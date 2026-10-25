//! Points in time, as seconds since the Unix epoch, and the calendar sums the adapters need.
//! Small enough not to need a date library: transcripts write UTC RFC 3339, and the rest is
//! days to dates and back.

use serde::Serialize;
use std::ops::{Add, Sub};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A point in time: seconds since the Unix epoch (UTC), fractions kept.
///
/// v0.1 keeps the representation as `f64` seconds: the same shape tvdash computed and
/// remote frames carry, and a plain JSON number on the wire. Sub-second (including
/// millisecond) precision survives canonical JSON round trips for RFC 3339-scale timestamps
/// (years 0-9999). Equality and ordering are `f64`'s, which is why the reducer compares
/// instants rather than requiring exact sums.
///
/// The library never reads the clock: every call that depends on the time takes `now`, so a
/// session's state is a pure function of what was fed to it and when you ask.
#[derive(Clone, Copy, Debug, Default, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Timestamp(f64);

impl Timestamp {
    /// The epoch itself.
    pub const EPOCH: Timestamp = Timestamp(0.0);

    pub const fn from_secs_f64(secs: f64) -> Self {
        Timestamp(secs)
    }

    pub const fn as_secs_f64(self) -> f64 {
        self.0
    }

    /// The same point as a `SystemTime` (the epoch, for times before it).
    pub fn to_system_time(self) -> SystemTime {
        UNIX_EPOCH + Duration::try_from_secs_f64(self.0).unwrap_or_default()
    }

    /// An RFC 3339 UTC time like `2026-10-09T20:02:33.310Z`; None if it doesn't read.
    pub fn parse_rfc3339(s: &str) -> Option<Self> {
        parse_rfc3339(s).map(Timestamp)
    }
}

impl From<SystemTime> for Timestamp {
    fn from(t: SystemTime) -> Self {
        Timestamp(epoch(t))
    }
}

impl Add<Duration> for Timestamp {
    type Output = Timestamp;
    fn add(self, d: Duration) -> Timestamp {
        Timestamp(self.0 + d.as_secs_f64())
    }
}

impl Sub<Duration> for Timestamp {
    type Output = Timestamp;
    fn sub(self, d: Duration) -> Timestamp {
        Timestamp(self.0 - d.as_secs_f64())
    }
}

/// Seconds since the epoch (0 for times before it).
pub(crate) fn epoch(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64()
}

/// Seconds since the epoch for an RFC 3339 timestamp like `2026-10-09T20:02:33.310Z` or
/// `2026-10-09T22:02:33.310+02:00`. A missing offset is UTC.
pub(crate) fn parse_rfc3339(s: &str) -> Option<f64> {
    let (date, time) = s.trim().split_once('T')?;
    let (time, offset) = split_offset(time)?;
    let mut d = date.split('-').map(|p| p.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    if d.next().is_some() {
        return None;
    }
    let (m, day) = (u32::try_from(m).ok()?, u32::try_from(day).ok()?);
    // RFC 3339 years are four digits. Anything outside that, and impossible dates or times,
    // is malformed: don't turn it into a surprising instant (or overflow the date arithmetic).
    if !(0..=9999).contains(&y) || !valid_date(y, m, day) {
        return None;
    }
    let mut t = time.split(':');
    let (hh, mm): (i64, i64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    let ss: f64 = t.next()?.parse().ok()?;
    if t.next().is_some() || !(0..=23).contains(&hh) || !(0..=59).contains(&mm) || !ss.is_finite() {
        return None;
    }
    // 60 is RFC 3339's leap second; fractional seconds stay below it.
    if !(0.0..=60.0).contains(&ss) {
        return None;
    }
    Some((days_from_civil(y, i64::from(m), i64::from(day)) * 86400 + hh * 3600 + mm * 60 - offset) as f64 + ss)
}

/// Split a time from its RFC 3339 offset. Returns the local time and the offset in seconds
/// east of UTC (`+02:00` is 7200). `Z`/`z` and a missing offset are UTC.
fn split_offset(time: &str) -> Option<(&str, i64)> {
    if let Some(time) = time.strip_suffix(['Z', 'z']) {
        return Some((time, 0));
    }
    let Some(i) = time.rfind(['+', '-']) else { return Some((time, 0)) };
    if i == 0 {
        return None;
    }
    let (time, offset) = time.split_at(i);
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    let offset = offset[1..].replace(':', "");
    if offset.len() != 4 || !offset.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hh: i64 = offset[..2].parse().ok()?;
    let mm: i64 = offset[2..].parse().ok()?;
    if hh > 23 || mm > 59 {
        return None;
    }
    Some((time, sign * (hh * 3600 + mm * 60)))
}

/// Days since the epoch of a date in the proleptic Gregorian calendar (Howard Hinnant's
/// algorithm). Not checked: 31 February is 3 March.
pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// The date (year, month, day) of a day counted from the epoch; `days_from_civil` undone.
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Whether `y`-`m`-`d` is a date that exists.
pub(crate) fn valid_date(y: i64, m: u32, d: u32) -> bool {
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rfc3339() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0.0));
        // Checked against Python's datetime.
        for (s, want) in [
            ("2026-10-09T20:02:33.310Z", 1791576153.31),
            ("2024-02-29T12:00:00Z", 1709208000.0),
            ("2000-03-01T00:00:59.5Z", 951868859.5),
        ] {
            assert!((parse_rfc3339(s).unwrap_or_else(|| panic!("{s} did not parse")) - want).abs() < 1e-6, "{s}");
        }
        // Offsets, with or without a colon, and a lowercase z.
        for (s, want) in [
            ("2026-10-09T22:02:33.310+02:00", 1791576153.31),
            ("2026-10-09T15:02:33.310-05:00", 1791576153.31),
            ("2026-10-09T22:02:33.310+0200", 1791576153.31),
            ("2026-10-09T20:02:33.310z", 1791576153.31),
        ] {
            assert!((parse_rfc3339(s).unwrap_or_else(|| panic!("{s} did not parse")) - want).abs() < 1e-6, "{s}");
        }
        assert_eq!(parse_rfc3339("not a time"), None);
        assert_eq!(parse_rfc3339("2026-10-09"), None);
        assert_eq!(parse_rfc3339("2026-10-09T20:02:33+2:00"), None);
        assert_eq!(parse_rfc3339("2026-10-09T20:02:33+99:00"), None);
        // Invalid dates, times and non-finite seconds are not turned into instants.
        for s in [
            "2026-00-01T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-02-29T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "2026-10-00T00:00:00Z",
            "2026-10-32T00:00:00Z",
            "2026-10-09T24:00:00Z",
            "2026-10-09T20:60:00Z",
            "2026-10-09T20:02:61Z",
            "2026-10-09T20:02:-1Z",
            "2026-10-09T20:02:NaN",
            "2026-10-09T20:02:inf",
            "999999999999999999-01-01T00:00:00Z",
        ] {
            assert_eq!(parse_rfc3339(s), None, "{s}");
        }
        assert!(parse_rfc3339("2024-02-29T23:59:60Z").is_some(), "a leap second is still a time");
    }

    #[test]
    fn days_and_dates_round_trip() {
        for days in (-800_000..800_000).step_by(997).chain([-1, 0, 1, 59, 60, 11_016, 20_513]) {
            let (y, m, d) = civil_from_days(days);
            assert!(valid_date(y, m, d), "{days}: {y}-{m}-{d}");
            assert_eq!(days_from_civil(y, i64::from(m), i64::from(d)), days);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert!(valid_date(2024, 2, 29) && !valid_date(2026, 2, 29) && !valid_date(2026, 10, 32));
        assert!(!valid_date(2026, 13, 1) && !valid_date(2026, 0, 1) && !valid_date(2026, 1, 0));
    }

    #[test]
    fn timestamps_serialize_as_plain_seconds() {
        for (secs, want) in [(1_792_756_800.123, "1792756800.123"), (0.0, "0.0"), (1.5, "1.5")] {
            let text = serde_json::to_string(&Timestamp::from_secs_f64(secs)).unwrap();
            assert_eq!(text, want);
            assert!(!text.contains(['e', 'E']), "no exponent for epoch seconds: {text}");
            let back: f64 = serde_json::from_str(&text).unwrap();
            assert_eq!(back, secs);
            assert_eq!(Timestamp::from_secs_f64(back), Timestamp::from_secs_f64(secs));
        }
    }

    #[test]
    fn timestamps_order_and_compare_as_seconds() {
        let epoch = Timestamp::EPOCH;
        let earlier = Timestamp::from_secs_f64(1_792_756_800.123);
        let later = Timestamp::from_secs_f64(1_792_756_800.124);
        assert!(epoch < earlier && earlier < later);
        assert!(later > earlier);
        assert_ne!(earlier, later);
        let mut times = [later, earlier, epoch];
        times.sort_by(|a, b| a.partial_cmp(b).expect("timestamps from a clock are ordered"));
        assert_eq!(times, [epoch, earlier, later]);
        // The same instant parsed with an offset is equal, not merely close.
        assert_eq!(Timestamp::parse_rfc3339("2026-10-23T14:00:00.123+02:00"), Some(earlier));
        assert_eq!(Timestamp::parse_rfc3339("2026-10-23T12:00:00.124Z"), Some(later));
    }

    #[test]
    fn timestamps_convert() {
        let t = Timestamp::from_secs_f64(1_700_000_000.5);
        assert_eq!(Timestamp::from(t.to_system_time()), t);
        assert_eq!((t + Duration::from_secs(10)).as_secs_f64(), 1_700_000_010.5);
        assert_eq!((t - Duration::from_millis(500)).as_secs_f64(), 1_700_000_000.0);
        assert_eq!(Timestamp::from_secs_f64(-5.0).to_system_time(), UNIX_EPOCH);
        assert_eq!(Timestamp::parse_rfc3339("1970-01-01T00:01:00Z"), Some(Timestamp::from_secs_f64(60.0)));
    }
}
