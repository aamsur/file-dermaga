//! Low-level helpers shared by the listing format parsers: whitespace
//! tokenizer, number/size/time/date field parsing and date construction
//! (two-digit years, `24:00`, the "recent file without a year" rule).

use super::months::parse_month;
use crate::datetime::{Accuracy, DateTime};

const MS_PER_DAY: i64 = 86_400_000;

/// A whitespace separated token and its byte offset in the line.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tok<'a> {
    pub s: &'a str,
    pub start: usize,
}

impl Tok<'_> {
    pub fn end(&self) -> usize {
        self.start + self.s.len()
    }
}

pub(crate) fn is_blank(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// Split on spaces and tabs.
pub(crate) fn tokenize(line: &str) -> Vec<Tok<'_>> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let start = i;
        while i < bytes.len() && bytes[i] != b' ' && bytes[i] != b'\t' {
            i += 1;
        }
        out.push(Tok {
            s: &line[start..i],
            start,
        });
    }
    out
}

/// The line from token `i` to the end, trailing blanks removed.
pub(crate) fn rest<'a>(line: &'a str, toks: &[Tok<'a>], i: usize) -> Option<&'a str> {
    let t = toks.get(i)?;
    let r = line.get(t.start..)?.trim_end_matches(is_blank);
    (!r.is_empty()).then_some(r)
}

pub(crate) fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Non-negative decimal integer (no sign, at most 18 digits).
pub(crate) fn uint(s: &str) -> Option<i64> {
    if !all_digits(s) || s.len() > 18 {
        return None;
    }
    s.parse().ok()
}

/// Size with optional binary unit suffix: `512`, `1.5M`, `33K`, `4kB`,
/// `2GiB`, `100B`. The fractional part is only allowed with a unit; the
/// result is truncated to whole bytes.
pub(crate) fn size_with_unit(s: &str) -> Option<i64> {
    let num_end = s
        .bytes()
        .position(|b| !(b.is_ascii_digit() || b == b'.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(num_end);
    let (int_part, frac_part) = match num.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (num, None),
    };
    if !all_digits(int_part) || int_part.len() > 18 {
        return None;
    }
    let shift = match unit.to_ascii_lowercase().as_str() {
        "" => 0,
        "b" => 0,
        "k" | "kb" | "kib" => 10,
        "m" | "mb" | "mib" => 20,
        "g" | "gb" | "gib" => 30,
        "t" | "tb" | "tib" => 40,
        "p" | "pb" | "pib" => 50,
        "e" | "eb" | "eib" => 60,
        _ => return None,
    };
    let int: i128 = int_part.parse().ok()?;
    let mut value = int << shift;
    if let Some(f) = frac_part {
        if shift == 0 || !all_digits(f) {
            return None;
        }
        let f = &f[..f.len().min(9)];
        let frac: i128 = f.parse().ok()?;
        let scale = 10i128.pow(f.len() as u32);
        value += (frac << shift) / scale;
    }
    i64::try_from(value).ok()
}

/// Digits with `,` or `.` thousands separators (`1,234,567`, `1.234`).
pub(crate) fn size_with_separators(s: &str) -> Option<i64> {
    if s.starts_with([',', '.']) || s.ends_with([',', '.']) {
        return None;
    }
    let digits: String = s.chars().filter(|&c| c != ',' && c != '.').collect();
    if digits.len() + s.matches([',', '.']).count() != s.len() {
        return None;
    }
    uint(&digits)
}

/// Time of day as written in a listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TimeOfDay {
    pub h: u32,
    pub m: u32,
    pub s: Option<u32>,
    pub ms: Option<u32>,
}

/// `H:MM`, `HH:MM`, `HH:MM:SS`, `HH:MM:SS.fff` (any number of fraction
/// digits). Hour may be 24 only for `24:00[:00]`.
pub(crate) fn time_of_day(s: &str) -> Option<TimeOfDay> {
    let mut parts = s.splitn(3, ':');
    let h = parts.next()?;
    let m = parts.next()?;
    let sec = parts.next();
    if h.is_empty() || h.len() > 2 || m.len() != 2 || !all_digits(h) || !all_digits(m) {
        return None;
    }
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    let (s, ms) = match sec {
        None => (None, None),
        Some(x) => {
            let (whole, frac) = match x.split_once(['.', ',']) {
                Some((w, f)) => (w, Some(f)),
                None => (x, None),
            };
            if whole.len() != 2 || !all_digits(whole) {
                return None;
            }
            let ms = match frac {
                None => None,
                Some(f) => {
                    if !all_digits(f) {
                        return None;
                    }
                    let mut v = 0u32;
                    for i in 0..3 {
                        v = v * 10 + f.as_bytes().get(i).map_or(0, |b| u32::from(b - b'0'));
                    }
                    Some(v)
                }
            };
            (Some(whole.parse().ok()?), ms)
        }
    };
    if m > 59 || s.is_some_and(|s| s > 59) {
        return None;
    }
    if h > 24 || (h == 24 && (m != 0 || s.unwrap_or(0) != 0 || ms.unwrap_or(0) != 0)) {
        return None;
    }
    Some(TimeOfDay { h, m, s, ms })
}

/// Two-digit years: 00–49 → 2000–2049, 50–99 → 1950–1999.
pub(crate) fn expand_year(y: i64, digits: usize) -> i64 {
    if digits <= 2 {
        if y < 50 {
            2000 + y
        } else {
            1900 + y
        }
    } else {
        y
    }
}

/// Parse a year token of 2 or 4 digits (2-digit years expanded).
pub(crate) fn year_any(s: &str) -> Option<i64> {
    if !all_digits(s) || !(s.len() == 2 || s.len() == 4) {
        return None;
    }
    Some(expand_year(s.parse().ok()?, s.len()))
}

/// Build a timestamp in UTC from listing fields. `24:00` is the end of the
/// day; fractional seconds give Milliseconds accuracy.
pub(crate) fn make_dt(y: i64, mo: u32, d: u32, t: Option<TimeOfDay>) -> Option<DateTime> {
    let (mo, d) = (i64::from(mo), i64::from(d));
    let Some(t) = t else {
        return DateTime::from_utc(y, mo, d, None, None, None);
    };
    let (h, next_day) = if t.h == 24 { (0, true) } else { (t.h, false) };
    let mut dt = DateTime::from_utc(
        y,
        mo,
        d,
        Some(i64::from(h)),
        Some(i64::from(t.m)),
        t.s.map(i64::from),
    )?;
    if let (Some(ms), Some(_)) = (t.ms, t.s) {
        dt = DateTime::from_unix_millis(dt.unix_millis()? + i64::from(ms), Accuracy::Milliseconds);
    }
    if next_day {
        dt.add_millis(MS_PER_DAY);
    }
    Some(dt)
}

/// Date without a year (`Mon DD HH:MM`): the current UTC year unless that
/// is more than one day in the future, then the previous year.
pub(crate) fn recent_dt(mo: u32, d: u32, t: Option<TimeOfDay>, now_ms: i64) -> Option<DateTime> {
    let now = DateTime::from_unix_millis(now_ms, Accuracy::Milliseconds);
    let year = now.to_tm()?.year;
    if let Some(dt) = make_dt(year, mo, d, t) {
        if dt.unix_millis()? <= now_ms.saturating_add(MS_PER_DAY) {
            return Some(dt);
        }
    }
    make_dt(year - 1, mo, d, t)
}

/// Remove any trailing non-ASCII characters (CJK/Korean unit suffixes, or
/// legacy Asian encodings that arrived as Latin-1). Returns the stripped
/// text and whether anything was removed.
pub(crate) fn strip_non_ascii_suffix(s: &str) -> (&str, bool) {
    let t = s.trim_end_matches(|c: char| !c.is_ascii());
    (t, t.len() != s.len())
}

/// Digits followed by at least one non-ASCII character (`5月`, `21日`).
fn suffixed_number(s: &str) -> Option<&str> {
    let (t, stripped) = strip_non_ascii_suffix(s);
    (stripped && all_digits(t)).then_some(t)
}

fn strip_punct(s: &str) -> &str {
    s.trim_end_matches(['.', ','])
}

/// Day of month 1–31, optionally followed by `.`/`,` or a non-ASCII suffix.
pub(crate) fn day_tok(s: &str) -> Option<u32> {
    let s = strip_punct(strip_non_ascii_suffix(s).0);
    if !all_digits(s) || s.len() > 2 {
        return None;
    }
    let d: u32 = s.parse().ok()?;
    (1..=31).contains(&d).then_some(d)
}

/// Four-digit year, optionally followed by `.`/`,` or a non-ASCII suffix.
pub(crate) fn year_tok(s: &str) -> Option<i64> {
    let s = strip_punct(strip_non_ascii_suffix(s).0);
    if !all_digits(s) || s.len() != 4 {
        return None;
    }
    s.parse().ok()
}

/// Year that carries a non-ASCII suffix (`2022年`, `2022년`).
fn suffixed_year(s: &str) -> Option<i64> {
    let base = suffixed_number(s)?;
    if base.len() != 4 {
        return None;
    }
    base.parse().ok()
}

/// Numeric date with one separator out of `-`, `/`, `.`: `MM-DD-YY[YY]`,
/// `YYYY-MM-DD`, `DD.MM.YYYY`; a first number above 12 is the day.
/// Three-digit years count from 1900 (`06-15-104` is 2004).
/// Returns (year, month, day).
pub(crate) fn numeric_date(s: &str) -> Option<(i64, u32, u32)> {
    let sep = s.chars().find(|c| matches!(c, '-' | '/' | '.'))?;
    let mut parts = s.split(sep);
    let (a, b, c) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || !all_digits(a) || !all_digits(b) || !all_digits(c) {
        return None;
    }
    if a.len() > 4 || b.len() > 2 || c.len() > 4 {
        return None;
    }
    let (na, nb, nc): (u32, u32, u32) = (a.parse().ok()?, b.parse().ok()?, c.parse().ok()?);
    if a.len() == 4 {
        if c.len() > 2 {
            return None;
        }
        return Some((i64::from(na), nb, nc));
    }
    if a.len() > 2 {
        return None;
    }
    let year = match c.len() {
        2 | 4 => expand_year(i64::from(nc), c.len()),
        3 => 1900 + i64::from(nc),
        _ => return None,
    };
    if sep == '.' || na > 12 {
        Some((year, nb, na))
    } else {
        Some((year, na, nb))
    }
}

/// Strict `YYYY<sep>MM<sep>DD`.
pub(crate) fn ymd_date(s: &str, sep: char) -> Option<(i64, u32, u32)> {
    let mut parts = s.split(sep);
    let (a, b, c) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || a.len() != 4 || b.len() != 2 || c.len() != 2 {
        return None;
    }
    if !all_digits(a) || !all_digits(b) || !all_digits(c) {
        return None;
    }
    Some((a.parse().ok()?, b.parse().ok()?, c.parse().ok()?))
}

/// `DD-Mon-YYYY` / `DD-Mon-YY` (month by name).
pub(crate) fn day_mon_year(s: &str) -> Option<(i64, u32, u32)> {
    let mut parts = s.split('-');
    let (d, m, y) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || !all_digits(d) || d.len() > 2 {
        return None;
    }
    let month = parse_month(m, false)?;
    Some((year_any(y)?, month, d.parse().ok()?))
}

/// `Mon-DD-YYYY` (month by name), as used by VxWorks.
pub(crate) fn mon_day_year(s: &str) -> Option<(i64, u32, u32)> {
    let mut parts = s.split('-');
    let (m, d, y) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || !all_digits(d) || d.len() > 2 {
        return None;
    }
    let month = parse_month(m, false)?;
    Some((year_tok(y)?, month, d.parse().ok()?))
}

/// `MM-DD` (no year).
fn month_day(s: &str) -> Option<(u32, u32)> {
    let (m, d) = s.split_once('-')?;
    if !all_digits(m) || !all_digits(d) || m.len() > 2 || d.len() > 2 {
        return None;
    }
    let (m, d): (u32, u32) = (m.parse().ok()?, d.parse().ok()?);
    ((1..=12).contains(&m) && (1..=31).contains(&d)).then_some((m, d))
}

/// Timezone token such as `+0200` or `-0530`; returns the offset in minutes.
fn zone_offset(s: &str) -> Option<i64> {
    let (sign, digits) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    if digits.len() != 4 || !all_digits(digits) {
        return None;
    }
    let h: i64 = digits[..2].parse().ok()?;
    let m: i64 = digits[2..].parse().ok()?;
    (h <= 14 && m < 60).then_some(sign * (h * 60 + m))
}

/// The date forms used by `ls -l` style listings, starting at token `i`.
/// Returns the timestamp and the index of the first token after the date.
/// At least one token (the name) must follow the date.
pub(crate) fn unix_date(toks: &[Tok<'_>], i: usize, now_ms: i64) -> Option<(DateTime, usize)> {
    let n = toks.len();
    let get = |k: usize| toks.get(k).map(|t| t.s);
    let t0 = get(i)?;

    // Token at `k` after a month and day: a time (recent) or a year,
    // optionally followed by a time.
    let tail = |mo: u32, d: u32, k: usize| -> Option<(DateTime, usize)> {
        let x = get(k)?;
        if let Some(t) = time_of_day(x) {
            if k + 1 < n {
                return Some((recent_dt(mo, d, Some(t), now_ms)?, k + 1));
            }
            return None;
        }
        let y = year_tok(x)?;
        if let Some(t) = get(k + 1).and_then(time_of_day) {
            if k + 2 < n {
                if let Some(dt) = make_dt(y, mo, d, Some(t)) {
                    return Some((dt, k + 2));
                }
            }
        }
        if k + 1 < n {
            return Some((make_dt(y, mo, d, None)?, k + 1));
        }
        None
    };
    // A full date at token `k`, optionally followed by a time [and zone].
    let dated = |y: i64, mo: u32, d: u32, k: usize| -> Option<(DateTime, usize)> {
        if let Some(t) = get(k).and_then(time_of_day) {
            if let Some(z) = get(k + 1).and_then(zone_offset) {
                if k + 2 < n {
                    let mut dt = make_dt(y, mo, d, Some(t))?;
                    dt.add_millis(-z * 60_000);
                    return Some((dt, k + 2));
                }
            }
            if k + 1 < n {
                return Some((make_dt(y, mo, d, Some(t))?, k + 1));
            }
        }
        if k < n {
            return Some((make_dt(y, mo, d, None)?, k));
        }
        None
    };

    // Mon DD (time|year)
    if let Some(mo) = parse_month(t0, false) {
        if let Some(d) = get(i + 1).and_then(day_tok) {
            if let Some(r) = tail(mo, d, i + 2) {
                return Some(r);
            }
        }
    }
    // DD Mon (time|year)
    if let Some(d) = day_tok(t0) {
        if let Some(mo) = get(i + 1).and_then(|s| parse_month(s, false)) {
            if let Some(r) = tail(mo, d, i + 2) {
                return Some(r);
            }
        }
    }
    // M D (time|year), numeric month first
    if all_digits(t0) && t0.len() <= 2 {
        if let (Some(mo), Some(d)) = (
            parse_month(t0, true),
            get(i + 1).filter(|s| all_digits(s)).and_then(day_tok),
        ) {
            if let Some(r) = tail(mo, d, i + 2) {
                return Some(r);
            }
        }
    }
    // MM-DD (time|year)
    if let Some((mo, d)) = month_day(t0) {
        if let Some(r) = tail(mo, d, i + 1) {
            return Some(r);
        }
    }
    // YYYY年 MM月 DD日 [time], or YYYY MM月 DD [time]
    let year = suffixed_year(t0)
        .or_else(|| year_tok(t0).filter(|_| get(i + 1).and_then(suffixed_number).is_some()));
    if let Some(y) = year {
        if let (Some(mo), Some(d)) = (
            get(i + 1).and_then(|s| parse_month(s, false)),
            get(i + 2).and_then(day_tok),
        ) {
            if let Some(r) = dated(y, mo, d, i + 3) {
                return Some(r);
            }
        }
    }
    // YYYY-MM-DD, YYYY/MM/DD, YYYY.MM.DD [time [zone]]
    for sep in ['-', '/', '.'] {
        if let Some((y, mo, d)) = ymd_date(t0, sep) {
            return dated(y, mo, d, i + 1);
        }
    }
    // DD-mon-YY[YY] [time]
    if let Some((y, mo, d)) = day_mon_year(t0) {
        return dated(y, mo, d, i + 1);
    }
    // MM-DD-YY HH:MM
    if t0.contains('-') {
        if let Some((y, mo, d)) = numeric_date(t0) {
            let t = get(i + 1).and_then(time_of_day)?;
            if i + 2 < n {
                return Some((make_dt(y, mo, d, Some(t))?, i + 2));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(size_with_unit("512"), Some(512));
        assert_eq!(size_with_unit("1.5M"), Some(1_572_864));
        assert_eq!(size_with_unit("33K"), Some(33 * 1024));
        assert_eq!(size_with_unit("2G"), Some(2 << 30));
        assert_eq!(size_with_unit("4kB"), Some(4096));
        assert_eq!(size_with_unit("1.3K"), Some(1331));
        assert_eq!(size_with_unit("1.5"), None);
        assert_eq!(size_with_unit("K"), None);
        assert_eq!(size_with_unit("12x"), None);
        assert_eq!(size_with_unit("99999999999999999E"), None);
        assert_eq!(size_with_separators("1,234,567"), Some(1_234_567));
        assert_eq!(size_with_separators("1.234"), Some(1234));
        assert_eq!(size_with_separators(",1"), None);
        assert_eq!(size_with_separators("1x"), None);
    }

    #[test]
    fn times() {
        assert_eq!(
            time_of_day("09:15"),
            Some(TimeOfDay {
                h: 9,
                m: 15,
                s: None,
                ms: None
            })
        );
        assert_eq!(time_of_day("9:15:01").unwrap().s, Some(1));
        assert_eq!(time_of_day("09:15:22.5").unwrap().ms, Some(500));
        assert!(time_of_day("24:00").is_some());
        assert!(time_of_day("24:01").is_none());
        assert!(time_of_day("10:60").is_none());
        assert!(time_of_day("1015").is_none());
        assert!(time_of_day("10:5").is_none());
    }

    #[test]
    fn numeric_dates() {
        assert_eq!(numeric_date("03-14-24"), Some((2024, 3, 14)));
        assert_eq!(numeric_date("14.03.2024"), Some((2024, 3, 14)));
        assert_eq!(numeric_date("25/12/2022"), Some((2022, 12, 25)));
        assert_eq!(numeric_date("2023-07-04"), Some((2023, 7, 4)));
        assert_eq!(numeric_date("11-02-99"), Some((1999, 11, 2)));
        assert_eq!(numeric_date("1-2"), None);
        assert_eq!(numeric_date("1-2-3-4"), None);
    }
}
