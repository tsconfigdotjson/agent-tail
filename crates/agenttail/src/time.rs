//! Points in time, as seconds since the Unix epoch, and the calendar sums the adapters need.
//! Small enough not to need a date library: transcripts write UTC RFC 3339, and the rest is
//! days to dates and back.

use serde::Serialize;
use std::ops::{Add, Sub};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A point in time: seconds since the Unix epoch (UTC), fractions kept.
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

/// Seconds since the epoch for an RFC 3339 UTC timestamp like 2026-10-09T20:02:33.310Z.
pub(crate) fn parse_rfc3339(s: &str) -> Option<f64> {
    let (date, time) = s.trim_end_matches('Z').split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let mut t = time.split(':');
    let (hh, mm): (i64, i64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    let ss: f64 = t.next()?.parse().ok()?;
    Some((days_from_civil(y, m, day) * 86400 + hh * 3600 + mm * 60) as f64 + ss)
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
            assert!((parse_rfc3339(s).unwrap() - want).abs() < 1e-6, "{s}");
        }
        assert_eq!(parse_rfc3339("not a time"), None);
        assert_eq!(parse_rfc3339("2026-10-09"), None);
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
    fn timestamps_convert() {
        let t = Timestamp::from_secs_f64(1_700_000_000.5);
        assert_eq!(Timestamp::from(t.to_system_time()), t);
        assert_eq!((t + Duration::from_secs(10)).as_secs_f64(), 1_700_000_010.5);
        assert_eq!((t - Duration::from_millis(500)).as_secs_f64(), 1_700_000_000.0);
        assert_eq!(Timestamp::from_secs_f64(-5.0).to_system_time(), UNIX_EPOCH);
        assert_eq!(Timestamp::parse_rfc3339("1970-01-01T00:01:00Z"), Some(Timestamp::from_secs_f64(60.0)));
    }
}
