//! Timestamps that remember how precise they are.
//!
//! [`DateTime`] stores an instant as milliseconds since the Unix epoch (UTC)
//! together with an [`Accuracy`]. Directory listings often only give a date,
//! or a time without seconds; keeping the accuracy lets callers compare and
//! display such values without inventing precision that was never there.
//! Calendar math uses the proleptic Gregorian calendar and works for any
//! year in a wide range around the epoch (including years before 1970).

use std::cmp::Ordering;
use std::fmt;

/// How precise a [`DateTime`] is, ordered from lowest to highest precision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Accuracy {
    Days,
    Hours,
    Minutes,
    Seconds,
    Milliseconds,
}

impl Accuracy {
    /// Length of one unit of this accuracy in milliseconds.
    fn unit_millis(self) -> i64 {
        match self {
            Accuracy::Days => MS_PER_DAY,
            Accuracy::Hours => 3_600_000,
            Accuracy::Minutes => 60_000,
            Accuracy::Seconds => 1_000,
            Accuracy::Milliseconds => 1,
        }
    }
}

const MS_PER_DAY: i64 = 86_400_000;
/// Years outside this range are rejected so that all arithmetic stays well
/// inside `i64`.
const MAX_ABS_YEAR: i64 = 100_000_000;

/// A UTC instant with an associated accuracy, or "empty" (unknown).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DateTime {
    ms: i64,
    acc: Accuracy,
    set: bool,
}

/// Broken-down UTC representation of a [`DateTime`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tm {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub millisecond: u32,
}

impl Default for DateTime {
    fn default() -> Self {
        DateTime::empty()
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (no validation).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // 0..=399
    let mp = (month + 9) % 12; // March = 0
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // 0..=146096
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

impl DateTime {
    /// The empty (unknown) timestamp.
    pub const fn empty() -> Self {
        DateTime {
            ms: 0,
            acc: Accuracy::Days,
            set: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        !self.set
    }

    pub fn clear(&mut self) {
        *self = DateTime::empty();
    }

    /// Accuracy of the value (`Days` for an empty value).
    pub fn accuracy(&self) -> Accuracy {
        self.acc
    }

    /// Milliseconds since 1970-01-01T00:00:00Z; None when empty.
    pub fn unix_millis(&self) -> Option<i64> {
        if self.set {
            Some(self.ms)
        } else {
            None
        }
    }

    /// The current time with millisecond accuracy.
    pub fn now() -> Self {
        let ms = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
            Err(e) => i64::try_from(e.duration().as_millis())
                .map(|v| -v)
                .unwrap_or(i64::MIN),
        };
        DateTime::from_unix_millis(ms, Accuracy::Milliseconds)
    }

    pub fn from_unix_secs(secs: i64, acc: Accuracy) -> Self {
        DateTime::from_unix_millis(secs.saturating_mul(1000), acc)
    }

    /// The value is truncated (towards the past) to a whole unit of `acc`.
    pub fn from_unix_millis(ms: i64, acc: Accuracy) -> Self {
        let unit = acc.unit_millis();
        let truncated = ms.div_euclid(unit).checked_mul(unit).unwrap_or(ms);
        DateTime {
            ms: truncated,
            acc,
            set: true,
        }
    }

    /// Interpreted as UTC. `None` fields define the accuracy: no hour → Days,
    /// hour only → Hours, hour+minute → Minutes, all three → Seconds.
    /// Returns None for invalid dates/times (e.g. month 13, Feb 30, minute 60).
    pub fn from_utc(
        year: i64,
        month: i64,
        day: i64,
        hour: Option<i64>,
        minute: Option<i64>,
        second: Option<i64>,
    ) -> Option<Self> {
        if !(-MAX_ABS_YEAR..=MAX_ABS_YEAR).contains(&year) {
            return None;
        }
        if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
            return None;
        }
        let days = days_from_civil(year, month, day);
        let mut ms = days.checked_mul(MS_PER_DAY)?;
        let acc = match (hour, minute, second) {
            (None, _, _) => Accuracy::Days,
            (Some(h), None, _) => {
                if !(0..24).contains(&h) {
                    return None;
                }
                ms += h * 3_600_000;
                Accuracy::Hours
            }
            (Some(h), Some(mi), None) => {
                if !(0..24).contains(&h) || !(0..60).contains(&mi) {
                    return None;
                }
                ms += h * 3_600_000 + mi * 60_000;
                Accuracy::Minutes
            }
            (Some(h), Some(mi), Some(s)) => {
                if !(0..24).contains(&h) || !(0..60).contains(&mi) || !(0..60).contains(&s) {
                    return None;
                }
                ms += h * 3_600_000 + mi * 60_000 + s * 1000;
                Accuracy::Seconds
            }
        };
        Some(DateTime { ms, acc, set: true })
    }

    /// Date only (Days accuracy); empty if invalid.
    pub fn ymd(year: i64, month: i64, day: i64) -> Self {
        DateTime::from_utc(year, month, day, None, None, None).unwrap_or_default()
    }

    /// Date and time with Minutes accuracy; empty if invalid.
    pub fn ymd_hm(year: i64, month: i64, day: i64, h: i64, mi: i64) -> Self {
        DateTime::from_utc(year, month, day, Some(h), Some(mi), None).unwrap_or_default()
    }

    /// Date and time with Seconds accuracy; empty if invalid.
    pub fn ymd_hms(year: i64, month: i64, day: i64, h: i64, mi: i64, s: i64) -> Self {
        DateTime::from_utc(year, month, day, Some(h), Some(mi), Some(s)).unwrap_or_default()
    }

    /// `YYYYMMDD`, `YYYYMMDDHH`, `YYYYMMDDHHMM`, `YYYYMMDDHHMMSS`, optionally
    /// followed by `.` and fraction digits (→ Milliseconds). UTC.
    /// Used for MLSD `modify=` and FTP `MDTM` replies. None if malformed.
    ///
    /// A fraction is only accepted after a full `YYYYMMDDHHMMSS`. RFC 3659
    /// allows any number of fraction digits; digits beyond the third are
    /// ignored.
    pub fn parse_compact(s: &str) -> Option<Self> {
        let s = s.trim();
        let (main, frac) = match s.split_once('.') {
            Some((m, f)) => (m, Some(f)),
            None => (s, None),
        };
        if !main.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let num = |a: usize, b: usize| -> i64 {
            main[a..b]
                .bytes()
                .fold(0i64, |acc, d| acc * 10 + i64::from(d - b'0'))
        };
        let (y, mo, d) = match main.len() {
            8 | 10 | 12 | 14 => (num(0, 4), num(4, 6), num(6, 8)),
            _ => return None,
        };
        let hour = (main.len() >= 10).then(|| num(8, 10));
        let minute = (main.len() >= 12).then(|| num(10, 12));
        let second = (main.len() >= 14).then(|| num(12, 14));
        let dt = DateTime::from_utc(y, mo, d, hour, minute, second)?;
        match frac {
            None => Some(dt),
            Some(f) => {
                if main.len() != 14 || f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                let mut millis = 0i64;
                for i in 0..3 {
                    let digit = f.as_bytes().get(i).map_or(0, |b| i64::from(b - b'0'));
                    millis = millis * 10 + digit;
                }
                Some(DateTime::from_unix_millis(
                    dt.ms + millis,
                    Accuracy::Milliseconds,
                ))
            }
        }
    }

    /// UTC breakdown; None when empty.
    pub fn to_tm(&self) -> Option<Tm> {
        if !self.set {
            return None;
        }
        let days = self.ms.div_euclid(MS_PER_DAY);
        let rem = self.ms.rem_euclid(MS_PER_DAY);
        let (year, month, day) = civil_from_days(days);
        Some(Tm {
            year,
            month,
            day,
            hour: (rem / 3_600_000) as u32,
            minute: (rem / 60_000 % 60) as u32,
            second: (rem / 1000 % 60) as u32,
            millisecond: (rem % 1000) as u32,
        })
    }

    /// Compare at the lower of the two accuracies (2024-03-14 at Days accuracy
    /// equals 2024-03-14 09:15 at Minutes accuracy). Empty sorts first.
    pub fn compare(&self, other: &Self) -> Ordering {
        match (self.set, other.set) {
            (false, false) => Ordering::Equal,
            (false, true) => Ordering::Less,
            (true, false) => Ordering::Greater,
            (true, true) => {
                let unit = self.acc.min(other.acc).unit_millis();
                self.ms.div_euclid(unit).cmp(&other.ms.div_euclid(unit))
            }
        }
    }

    /// Shift by a duration; no-op when empty.
    pub fn add_millis(&mut self, ms: i64) {
        if self.set {
            self.ms = self.ms.saturating_add(ms);
        }
    }
}

impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(t) = self.to_tm() else {
            return f.write_str("<none>");
        };
        write!(f, "{:04}-{:02}-{:02}", t.year, t.month, t.day)?;
        match self.acc {
            Accuracy::Days => Ok(()),
            Accuracy::Hours => write!(f, " {:02}h", t.hour),
            Accuracy::Minutes => write!(f, " {:02}:{:02}", t.hour, t.minute),
            Accuracy::Seconds => write!(f, " {:02}:{:02}:{:02}", t.hour, t.minute, t.second),
            Accuracy::Milliseconds => write!(
                f,
                " {:02}:{:02}:{:02}.{:03}",
                t.hour, t.minute, t.second, t.millisecond
            ),
        }
    }
}

impl fmt::Debug for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_default() {
        let e = DateTime::empty();
        assert!(e.is_empty());
        assert_eq!(e, DateTime::default());
        assert_eq!(e.unix_millis(), None);
        assert_eq!(e.to_tm(), None);
        assert_eq!(e.to_string(), "<none>");
        let mut d = DateTime::ymd(2024, 3, 14);
        assert!(!d.is_empty());
        d.clear();
        assert!(d.is_empty());
        assert_eq!(d, DateTime::empty());
    }

    #[test]
    fn epoch_and_known_values() {
        assert_eq!(DateTime::ymd(1970, 1, 1).unix_millis(), Some(0));
        assert_eq!(
            DateTime::ymd_hms(2024, 3, 14, 9, 15, 22).unix_millis(),
            Some(1_710_407_722_000)
        );
        assert_eq!(
            DateTime::ymd_hms(2023, 11, 14, 22, 13, 20).unix_millis(),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            DateTime::ymd(2000, 3, 1).unix_millis(),
            Some(951_868_800_000)
        );
    }

    #[test]
    fn pre_1970() {
        assert_eq!(DateTime::ymd(1969, 12, 31).unix_millis(), Some(-MS_PER_DAY));
        let d = DateTime::ymd_hms(1969, 12, 31, 23, 59, 59);
        assert_eq!(d.unix_millis(), Some(-1000));
        let tm = d.to_tm().unwrap();
        assert_eq!(
            (tm.year, tm.month, tm.day, tm.hour, tm.minute, tm.second),
            (1969, 12, 31, 23, 59, 59)
        );
        let old = DateTime::ymd(1600, 2, 29);
        assert!(!old.is_empty());
        let tm = old.to_tm().unwrap();
        assert_eq!((tm.year, tm.month, tm.day), (1600, 2, 29));
        let neg = DateTime::from_unix_millis(-1, Accuracy::Milliseconds);
        assert_eq!(neg.to_string(), "1969-12-31 23:59:59.999");
        let bc = DateTime::ymd(-44, 3, 15);
        assert_eq!(bc.to_tm().unwrap().year, -44);
    }

    #[test]
    fn far_future() {
        let d = DateTime::ymd_hms(9999, 12, 31, 23, 59, 59);
        assert_eq!(d.to_string(), "9999-12-31 23:59:59");
        let d = DateTime::ymd(275_760, 9, 13);
        let tm = d.to_tm().unwrap();
        assert_eq!((tm.year, tm.month, tm.day), (275_760, 9, 13));
        assert!(DateTime::ymd(1_000_000_000, 1, 1).is_empty());
    }

    #[test]
    fn leap_years() {
        assert!(!DateTime::ymd(2024, 2, 29).is_empty());
        assert!(DateTime::ymd(2023, 2, 29).is_empty());
        assert!(DateTime::ymd(1900, 2, 29).is_empty());
        assert!(!DateTime::ymd(2000, 2, 29).is_empty());
        assert!(DateTime::ymd(2100, 2, 29).is_empty());
        let mut d = DateTime::ymd(2024, 2, 28);
        d.add_millis(MS_PER_DAY);
        assert_eq!(d, DateTime::ymd(2024, 2, 29));
        d.add_millis(MS_PER_DAY);
        assert_eq!(d, DateTime::ymd(2024, 3, 1));
    }

    #[test]
    fn invalid_components() {
        assert!(DateTime::from_utc(2024, 13, 1, None, None, None).is_none());
        assert!(DateTime::from_utc(2024, 0, 1, None, None, None).is_none());
        assert!(DateTime::from_utc(2024, 2, 30, None, None, None).is_none());
        assert!(DateTime::from_utc(2024, 4, 31, None, None, None).is_none());
        assert!(DateTime::from_utc(2024, 1, 0, None, None, None).is_none());
        assert!(DateTime::from_utc(2024, 1, 1, Some(24), Some(0), None).is_none());
        assert!(DateTime::from_utc(2024, 1, 1, Some(10), Some(60), None).is_none());
        assert!(DateTime::from_utc(2024, 1, 1, Some(10), Some(0), Some(60)).is_none());
        assert!(DateTime::from_utc(2024, 1, 1, Some(-1), None, None).is_none());
        assert!(DateTime::ymd_hm(2024, 1, 1, 10, 61).is_empty());
    }

    #[test]
    fn accuracy_from_fields() {
        let d = |h, mi, s| {
            DateTime::from_utc(2024, 3, 14, h, mi, s)
                .unwrap()
                .accuracy()
        };
        assert_eq!(d(None, None, None), Accuracy::Days);
        assert_eq!(d(Some(9), None, None), Accuracy::Hours);
        assert_eq!(d(Some(9), Some(15), None), Accuracy::Minutes);
        assert_eq!(d(Some(9), Some(15), Some(22)), Accuracy::Seconds);
        assert!(Accuracy::Days < Accuracy::Hours);
        assert!(Accuracy::Seconds < Accuracy::Milliseconds);
    }

    #[test]
    fn parse_compact_forms() {
        assert_eq!(
            DateTime::parse_compact("20240314"),
            Some(DateTime::ymd(2024, 3, 14))
        );
        let h = DateTime::parse_compact("2024031409").unwrap();
        assert_eq!(h.accuracy(), Accuracy::Hours);
        assert_eq!(h.to_string(), "2024-03-14 09h");
        assert_eq!(
            DateTime::parse_compact("202403140915"),
            Some(DateTime::ymd_hm(2024, 3, 14, 9, 15))
        );
        assert_eq!(
            DateTime::parse_compact("20240314091522"),
            Some(DateTime::ymd_hms(2024, 3, 14, 9, 15, 22))
        );
        let ms = DateTime::parse_compact("20231102170000.250").unwrap();
        assert_eq!(ms.accuracy(), Accuracy::Milliseconds);
        assert_eq!(ms.to_string(), "2023-11-02 17:00:00.250");
        assert_eq!(
            DateTime::parse_compact("20231102170000.5")
                .unwrap()
                .to_tm()
                .unwrap()
                .millisecond,
            500
        );
        assert_eq!(
            DateTime::parse_compact("20231102170000.12")
                .unwrap()
                .to_tm()
                .unwrap()
                .millisecond,
            120
        );
        assert_eq!(
            DateTime::parse_compact("20231102170000.123456")
                .unwrap()
                .to_tm()
                .unwrap()
                .millisecond,
            123
        );
    }

    #[test]
    fn parse_compact_malformed() {
        for s in [
            "",
            "2024",
            "2024031",
            "202403141",
            "20241314",
            "20240230",
            "20240314246000",
            "20240314091560",
            "2024031409152",
            "20240314091522.",
            "20240314091522.x",
            "20240314.5",
            "2024-03-14",
            "x0240314",
            "20240314091522.1.2",
            "+0240314",
        ] {
            assert_eq!(DateTime::parse_compact(s), None, "{s:?}");
        }
    }

    #[test]
    fn compare_lower_accuracy() {
        let day = DateTime::ymd(2024, 3, 14);
        let min = DateTime::ymd_hm(2024, 3, 14, 9, 15);
        assert_eq!(day.compare(&min), Ordering::Equal);
        assert_eq!(min.compare(&day), Ordering::Equal);
        assert_ne!(day, min);
        let later = DateTime::ymd_hm(2024, 3, 14, 9, 16);
        assert_eq!(min.compare(&later), Ordering::Less);
        assert_eq!(later.compare(&min), Ordering::Greater);
        let secs = DateTime::ymd_hms(2024, 3, 14, 9, 15, 59);
        assert_eq!(secs.compare(&min), Ordering::Equal);
        assert_eq!(DateTime::ymd(2024, 3, 15).compare(&min), Ordering::Greater);
        let e = DateTime::empty();
        assert_eq!(e.compare(&day), Ordering::Less);
        assert_eq!(day.compare(&e), Ordering::Greater);
        assert_eq!(e.compare(&DateTime::empty()), Ordering::Equal);
        // Pre-epoch values must truncate towards the past.
        let a = DateTime::ymd(1969, 12, 31);
        let b = DateTime::ymd_hm(1969, 12, 31, 12, 0);
        assert_eq!(a.compare(&b), Ordering::Equal);
    }

    #[test]
    fn display_all_accuracies() {
        assert_eq!(DateTime::ymd(2024, 3, 4).to_string(), "2024-03-04");
        assert_eq!(
            DateTime::from_utc(2024, 3, 4, Some(5), None, None)
                .unwrap()
                .to_string(),
            "2024-03-04 05h"
        );
        assert_eq!(
            DateTime::ymd_hm(2024, 3, 4, 5, 6).to_string(),
            "2024-03-04 05:06"
        );
        assert_eq!(
            DateTime::ymd_hms(2024, 3, 4, 5, 6, 7).to_string(),
            "2024-03-04 05:06:07"
        );
        assert_eq!(
            DateTime::from_unix_millis(1_710_407_722_042, Accuracy::Milliseconds).to_string(),
            "2024-03-14 09:15:22.042"
        );
        assert_eq!(format!("{:?}", DateTime::ymd(2024, 3, 4)), "2024-03-04");
    }

    #[test]
    fn unix_constructors_truncate() {
        let d = DateTime::from_unix_secs(1_710_407_722, Accuracy::Seconds);
        assert_eq!(d, DateTime::ymd_hms(2024, 3, 14, 9, 15, 22));
        let d = DateTime::from_unix_secs(1_710_407_722, Accuracy::Days);
        assert_eq!(d, DateTime::ymd(2024, 3, 14));
        let d = DateTime::from_unix_millis(-1, Accuracy::Days);
        assert_eq!(d, DateTime::ymd(1969, 12, 31));
        let _ = DateTime::from_unix_millis(i64::MIN, Accuracy::Days).to_tm();
        let _ = DateTime::from_unix_millis(i64::MAX, Accuracy::Days).to_string();
        let _ = DateTime::from_unix_secs(i64::MIN, Accuracy::Seconds).to_string();
    }

    #[test]
    fn add_millis_behaviour() {
        let mut e = DateTime::empty();
        e.add_millis(1000);
        assert!(e.is_empty());
        let mut d = DateTime::ymd_hm(2023, 12, 31, 23, 30);
        d.add_millis(60 * 60_000);
        assert_eq!(d, DateTime::ymd_hm(2024, 1, 1, 0, 30));
        d.add_millis(-2 * 60 * 60_000);
        assert_eq!(d, DateTime::ymd_hm(2023, 12, 31, 22, 30));
        let mut m = DateTime::from_unix_millis(i64::MAX - 5, Accuracy::Milliseconds);
        m.add_millis(100);
        assert_eq!(m.unix_millis(), Some(i64::MAX));
    }

    #[test]
    fn now_is_reasonable() {
        let n = DateTime::now();
        assert_eq!(n.accuracy(), Accuracy::Milliseconds);
        assert!(n.to_tm().unwrap().year >= 2024);
    }

    #[test]
    fn roundtrip_many_days() {
        let mut day = days_from_civil(1800, 1, 1);
        let end = days_from_civil(2200, 12, 31);
        while day <= end {
            let (y, m, d) = civil_from_days(day);
            assert_eq!(days_from_civil(y, i64::from(m), i64::from(d)), day);
            assert!(i64::from(d) <= days_in_month(y, i64::from(m)));
            day += 1;
        }
    }
}
