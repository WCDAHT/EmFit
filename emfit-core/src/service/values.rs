//! Value grammars: the right-hand side of a function term.
//!
//! `size:>10mb`, `dm:last3days`, `childcount:2..8`, `attrib:HS`. Every numeric
//! and date function shares one comparison shell ([`Compare`]) and differs only
//! in how it reads a single value, so `>=`, `..`, and the rest behave the same
//! everywhere.
//!
//! Everything here returns `Option`: an unreadable value is a warning and a
//! dropped filter, never a failed query (see `service::query`).
//!
//! **Dates are UTC**, matching the timestamps in the index and the Date
//! Modified column. `today` is the UTC day.

use chrono::{Datelike, Days, Months, NaiveDate, NaiveDateTime, NaiveTime, Utc, Weekday};

use crate::model::entry::EntryFlags;

/// One nanosecond less than a day - the span of a bare date.
const DAY_NANOS: i64 = 24 * 60 * 60 * 1_000_000_000;

/// The comparison shell every numeric and date function shares.
enum Compare<'t> {
    AtLeast(&'t str),
    AtMost(&'t str),
    /// `=value`, and a bare value for everything except sizes.
    Exact(&'t str),
    /// `start..end` or `start-end`; either side may be empty.
    Range(&'t str, &'t str),
    /// A value with no operator, which each function reads its own way.
    Bare(&'t str),
}

fn compare(text: &str) -> Compare<'_> {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix(">=").or_else(|| text.strip_prefix('>')) {
        return Compare::AtLeast(rest.trim());
    }
    if let Some(rest) = text.strip_prefix("<=").or_else(|| text.strip_prefix('<')) {
        return Compare::AtMost(rest.trim());
    }
    if let Some(rest) = text.strip_prefix('=') {
        return Compare::Exact(rest.trim());
    }
    if let Some((lo, hi)) = split_range(text) {
        return Compare::Range(lo, hi);
    }
    Compare::Bare(text)
}

/// Split on `..`, or on a `-` that isn't part of a date (`2024-01-01`).
fn split_range(text: &str) -> Option<(&str, &str)> {
    if let Some((lo, hi)) = text.split_once("..") {
        return Some((lo.trim(), hi.trim()));
    }
    // A lone `-` separates sizes (`10MB-1GB`); dates contain dashes, so only
    // treat it as a range when the text holds exactly one.
    if text.matches('-').count() == 1 {
        let (lo, hi) = text.split_once('-')?;
        let (lo, hi) = (lo.trim(), hi.trim());
        // ...and when it does not spell a year and a month. `2024-06` is that
        // month, not the range from 2024 to 6.
        if !year_month(lo, hi) {
            return Some((lo, hi));
        }
    }
    None
}

/// Whether these two halves read as `YYYY-MM`.
fn year_month(lo: &str, hi: &str) -> bool {
    let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    lo.len() == 4 && digits(lo) && (1..=2).contains(&hi.len()) && digits(hi)
}

// ---------------------------------------------------------------------------
// counts
// ---------------------------------------------------------------------------

/// A plain number filter: `len:8`, `parents:>3`, `childcount:2..8`.
/// A bare value means exactly that many.
pub fn count(text: &str) -> Option<(u64, u64)> {
    let number = |t: &str| t.trim().parse::<u64>().ok();
    match compare(text) {
        Compare::AtLeast(v) => Some((number(v)?, u64::MAX)),
        Compare::AtMost(v) => Some((0, number(v)?)),
        Compare::Exact(v) | Compare::Bare(v) => {
            let n = number(v)?;
            Some((n, n))
        }
        Compare::Range(lo, hi) => {
            let lo = if lo.is_empty() { 0 } else { number(lo)? };
            let hi = if hi.is_empty() { u64::MAX } else { number(hi)? };
            (lo <= hi).then_some((lo, hi))
        }
    }
}

/// A single number, for `count:` - a result limit rather than a filter.
pub fn limit(text: &str) -> Option<usize> {
    text.trim().parse::<usize>().ok()
}

// ---------------------------------------------------------------------------
// sizes
// ---------------------------------------------------------------------------

/// `>10MB`, `<=1GB`, `500kb..2mb`, the constants, or a bare `10MB`.
///
/// A bare size means "at least this big" - the question a space analyzer is
/// usually asked, and long-standing EmFit behavior. `=10mb` is the exact form.
pub fn size(text: &str) -> Option<(u64, u64)> {
    if let Some(range) = size_constant(text.trim()) {
        return Some(range);
    }
    match compare(text) {
        Compare::AtLeast(v) => Some((bytes(v)?, u64::MAX)),
        Compare::AtMost(v) => Some((0, bytes(v)?)),
        Compare::Exact(v) => {
            let n = bytes(v)?;
            Some((n, n))
        }
        Compare::Range(lo, hi) => {
            let lo = if lo.is_empty() { 0 } else { bytes(lo)? };
            let hi = if hi.is_empty() { u64::MAX } else { bytes(hi)? };
            (lo <= hi).then_some((lo, hi))
        }
        Compare::Bare(v) => Some((bytes(v)?, u64::MAX)),
    }
}

/// The named size bands, bounds as documented: each starts one byte above the
/// one below it.
fn size_constant(text: &str) -> Option<(u64, u64)> {
    const KB: u64 = 1 << 10;
    const MB: u64 = 1 << 20;
    let bands: &[(&str, u64, u64)] = &[
        ("empty", 0, 0),
        ("tiny", 1, 10 * KB),
        ("small", 10 * KB + 1, 100 * KB),
        ("medium", 100 * KB + 1, MB),
        ("large", MB + 1, 16 * MB),
        ("huge", 16 * MB + 1, 128 * MB),
        ("gigantic", 128 * MB + 1, u64::MAX),
    ];
    bands
        .iter()
        .find(|(name, _, _)| text.eq_ignore_ascii_case(name))
        .map(|&(_, lo, hi)| (lo, hi))
}

/// `10`, `10kb`, `1.5GB` - binary units, matching the display side.
fn bytes(text: &str) -> Option<u64> {
    let text = text.trim().to_ascii_lowercase();
    let digits = text.trim_end_matches(char::is_alphabetic);
    let multiplier: u64 = match &text[digits.len()..] {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1u64 << 40,
        _ => return None,
    };
    let number = digits.trim();
    if number.is_empty() {
        return None;
    }
    let value: f64 = number.parse().ok()?;
    if value < 0.0 {
        return None;
    }
    Some((value * multiplier as f64) as u64)
}

// ---------------------------------------------------------------------------
// dates
// ---------------------------------------------------------------------------

/// `2024-06-15`, `>2024`, `2024-01..2024-06`, `today`, `last7days`.
///
/// A bare value means the whole span it names: a year, a month, a day, or a
/// second, depending on how precisely it was written.
pub fn date(text: &str) -> Option<(i64, i64)> {
    match compare(text) {
        Compare::AtLeast(v) => Some((span(v)?.0, i64::MAX)),
        Compare::AtMost(v) => Some((i64::MIN, span(v)?.1)),
        Compare::Exact(v) | Compare::Bare(v) => span(v),
        Compare::Range(lo, hi) => {
            let lo = if lo.is_empty() { i64::MIN } else { span(lo)?.0 };
            let hi = if hi.is_empty() { i64::MAX } else { span(hi)?.1 };
            (lo <= hi).then_some((lo, hi))
        }
    }
}

/// The inclusive nanosecond span one date expression covers.
fn span(text: &str) -> Option<(i64, i64)> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    relative(text).or_else(|| absolute(text))
}

/// `today`, `thismonth`, `last7days`, `march`, `friday`.
fn relative(text: &str) -> Option<(i64, i64)> {
    let text = text.to_ascii_lowercase();
    let today = Utc::now().date_naive();

    match text.as_str() {
        "today" => return Some(day_span(today)),
        "yesterday" => return Some(day_span(today.checked_sub_days(Days::new(1))?)),
        "tomorrow" => return Some(day_span(today.checked_add_days(Days::new(1))?)),
        _ => {}
    }

    if let Some((shift, unit)) = whole_unit(&text) {
        return match unit {
            "week" => {
                let monday = today.week(Weekday::Mon).first_day();
                let start = shift_days(monday, shift * 7)?;
                Some((day_span(start).0, day_span(shift_days(start, 6)?).1))
            }
            "month" => {
                let first = today.with_day(1)?;
                let start = shift_months(first, shift)?;
                let end = shift_months(start, 1)?.checked_sub_days(Days::new(1))?;
                Some((day_span(start).0, day_span(end).1))
            }
            "year" => {
                let start = NaiveDate::from_ymd_opt(today.year() + shift, 1, 1)?;
                let end = NaiveDate::from_ymd_opt(start.year(), 12, 31)?;
                Some((day_span(start).0, day_span(end).1))
            }
            _ => None,
        };
    }

    if let Some((count, unit, forward)) = counted_unit(&text) {
        return counted_span(today, count, unit, forward);
    }
    if let Some(month) = month_number(&text) {
        // The most recent month with that name, this year or last.
        let year = if month <= today.month() {
            today.year()
        } else {
            today.year() - 1
        };
        let start = NaiveDate::from_ymd_opt(year, month, 1)?;
        let end = shift_months(start, 1)?.checked_sub_days(Days::new(1))?;
        return Some((day_span(start).0, day_span(end).1));
    }
    if let Some(weekday) = weekday(&text) {
        // The most recent day with that name, today included.
        let back =
            (today.weekday().num_days_from_monday() + 7 - weekday.num_days_from_monday()) % 7;
        return Some(day_span(today.checked_sub_days(Days::new(back.into()))?));
    }
    None
}

/// `thisweek`, `lastmonth`, `nextyear` - a whole calendar unit, and how many
/// of them away from the current one it sits.
fn whole_unit(text: &str) -> Option<(i32, &'static str)> {
    let prefixes: &[(&str, i32)] = &[
        ("last", -1),
        ("past", -1),
        ("prev", -1),
        ("current", 0),
        ("this", 0),
        ("coming", 1),
        ("next", 1),
    ];
    for (prefix, shift) in prefixes {
        if let Some(rest) = text.strip_prefix(prefix) {
            for unit in ["week", "month", "year"] {
                if rest == unit {
                    return Some((*shift, unit));
                }
            }
        }
    }
    None
}

/// `last3days`, `next2weeks` - a count of units either side of now.
fn counted_unit(text: &str) -> Option<(u64, &str, bool)> {
    let forward = ["coming", "next"];
    let backward = ["last", "past", "prev"];

    let (rest, forward) = forward
        .iter()
        .find_map(|p| text.strip_prefix(p).map(|r| (r, true)))
        .or_else(|| {
            backward
                .iter()
                .find_map(|p| text.strip_prefix(p).map(|r| (r, false)))
        })?;

    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let unit = &rest[digits.len()..];
    Some((digits.parse().ok()?, unit, forward))
}

/// The span `last3days` and friends cover: whole days back from today, or a
/// rolling window for the sub-day units.
fn counted_span(today: NaiveDate, count: u64, unit: &str, forward: bool) -> Option<(i64, i64)> {
    let sign: i64 = if forward { 1 } else { -1 };
    let n = i64::try_from(count).ok()?;
    let steps = i32::try_from(count).ok()?;
    let step_sign = sign as i32;

    let seconds = match unit {
        "seconds" | "secs" | "second" | "sec" => Some(1),
        "minutes" | "mins" | "minute" | "min" => Some(60),
        "hours" | "hour" => Some(3600),
        _ => None,
    };
    if let Some(per) = seconds {
        let now = Utc::now().timestamp_nanos_opt()?;
        let delta = n.checked_mul(per)?.checked_mul(1_000_000_000)?;
        let other = now.checked_add(sign * delta)?;
        return Some((now.min(other), now.max(other)));
    }

    // Day-and-larger units cover whole days, ending today (or starting today
    // when looking forward).
    let edge = match unit {
        "days" | "day" => shift_days(today, step_sign * (steps - 1))?,
        "weeks" | "week" => shift_days(today, step_sign * (steps * 7 - 1))?,
        "months" | "month" => shift_months(today, step_sign * steps)?,
        "years" | "year" => shift_months(today, step_sign * steps * 12)?,
        _ => return None,
    };
    let (a, b) = (day_span(today), day_span(edge));
    Some((a.0.min(b.0), a.1.max(b.1)))
}

fn shift_days(date: NaiveDate, days: i32) -> Option<NaiveDate> {
    let count = Days::new(days.unsigned_abs().into());
    if days < 0 {
        date.checked_sub_days(count)
    } else {
        date.checked_add_days(count)
    }
}

fn shift_months(date: NaiveDate, months: i32) -> Option<NaiveDate> {
    let count = Months::new(months.unsigned_abs());
    if months < 0 {
        date.checked_sub_months(count)
    } else {
        date.checked_add_months(count)
    }
}

fn month_number(text: &str) -> Option<u32> {
    const NAMES: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    NAMES
        .iter()
        .position(|name| *name == text || name.starts_with(text) && text.len() == 3)
        .map(|at| at as u32 + 1)
}

fn weekday(text: &str) -> Option<Weekday> {
    const NAMES: [(&str, Weekday); 7] = [
        ("monday", Weekday::Mon),
        ("tuesday", Weekday::Tue),
        ("wednesday", Weekday::Wed),
        ("thursday", Weekday::Thu),
        ("friday", Weekday::Fri),
        ("saturday", Weekday::Sat),
        ("sunday", Weekday::Sun),
    ];
    NAMES
        .iter()
        .find(|(name, _)| *name == text || name.starts_with(text) && text.len() == 3)
        .map(|&(_, day)| day)
}

/// `2024`, `2024-06`, `2024-06-15`, `2024-06-15T09:30`, and the compact forms.
///
/// ISO ordering only: `06/15/2024` is ambiguous across locales, and a search
/// that silently reads a date the other way round is worse than one that says
/// it did not understand.
fn absolute(text: &str) -> Option<(i64, i64)> {
    let (date_part, time_part) = match text.split_once(['T', 't']) {
        Some((d, t)) => (d, Some(t)),
        None => (text, None),
    };

    let digits: Vec<&str> = date_part.split(['-', '/']).collect();
    let (year, month, day) = match digits.as_slice() {
        [y] if y.len() == 4 => (y.parse().ok()?, None, None),
        // Compact: YYYYMM and YYYYMMDD.
        [y] if y.len() == 6 => (y[..4].parse().ok()?, Some(y[4..].parse().ok()?), None),
        [y] if y.len() == 8 => (
            y[..4].parse().ok()?,
            Some(y[4..6].parse().ok()?),
            Some(y[6..].parse().ok()?),
        ),
        [y, m] => (y.parse().ok()?, Some(m.parse().ok()?), None),
        [y, m, d] => (
            y.parse().ok()?,
            Some(m.parse().ok()?),
            Some(d.parse().ok()?),
        ),
        _ => return None,
    };

    let Some(month) = month else {
        let start = NaiveDate::from_ymd_opt(year, 1, 1)?;
        return Some((day_span(start).0, day_span(start.with_ordinal(365)?).1));
    };
    let Some(day) = day else {
        let start = NaiveDate::from_ymd_opt(year, month, 1)?;
        let end = shift_months(start, 1)?.checked_sub_days(Days::new(1))?;
        return Some((day_span(start).0, day_span(end).1));
    };

    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let Some(time) = time_part else {
        return Some(day_span(date));
    };
    time_span(date, time)
}

/// `hh`, `hh:mm`, `hh:mm:ss` - the span is whatever precision was written.
fn time_span(date: NaiveDate, text: &str) -> Option<(i64, i64)> {
    let parts: Vec<&str> = text.split(':').collect();
    let (hour, minute, second, width) = match parts.as_slice() {
        [h] => (h.parse().ok()?, 0, 0, 3600),
        [h, m] => (h.parse().ok()?, m.parse().ok()?, 0, 60),
        [h, m, s] => (
            h.parse().ok()?,
            m.parse().ok()?,
            s.split('.').next()?.parse().ok()?,
            1,
        ),
        _ => return None,
    };
    let at = date.and_time(NaiveTime::from_hms_opt(hour, minute, second)?);
    let start = nanos(at)?;
    Some((start, start + width * 1_000_000_000 - 1))
}

/// Midnight to the last nanosecond of the day, UTC.
fn day_span(date: NaiveDate) -> (i64, i64) {
    let start = date
        .and_hms_opt(0, 0, 0)
        .and_then(nanos)
        // Beyond what nanosecond timestamps can hold (before 1677 or after
        // 2262): clamp rather than drop the filter.
        .unwrap_or(i64::MIN);
    (start, start.saturating_add(DAY_NANOS - 1))
}

fn nanos(at: NaiveDateTime) -> Option<i64> {
    at.and_utc().timestamp_nanos_opt()
}

// ---------------------------------------------------------------------------
// attributes
// ---------------------------------------------------------------------------

/// Attribute letters EmFit records, and what they mean.
const ATTRIBUTES: &[(char, EntryFlags)] = &[
    ('C', EntryFlags::COMPRESSED),
    ('D', EntryFlags::DIRECTORY),
    ('H', EntryFlags::HIDDEN),
    ('L', EntryFlags::REPARSE),
    ('P', EntryFlags::SPARSE),
    ('S', EntryFlags::SYSTEM),
];

/// Letters the index has no bit for. Recognized so the parser can say so
/// rather than silently filtering on nothing.
const UNRECORDED: &[char] = &['A', 'E', 'I', 'N', 'O', 'R', 'T', 'V'];

/// Parse `attrib:HS` into the bits that must be set, plus any letters this
/// index cannot answer for.
pub fn attributes(text: &str) -> (EntryFlags, Vec<char>) {
    let mut wanted = EntryFlags::empty();
    let mut unknown = Vec::new();
    for c in text.chars() {
        let upper = c.to_ascii_uppercase();
        match ATTRIBUTES.iter().find(|(letter, _)| *letter == upper) {
            Some(&(_, flag)) => wanted = wanted.union(flag),
            None => unknown.push(c),
        }
    }
    unknown.retain(|c| !c.is_whitespace());
    (wanted, unknown)
}

/// Whether a letter is a real attribute EmFit simply does not record, as
/// opposed to a typo.
pub fn is_unrecorded_attribute(c: char) -> bool {
    UNRECORDED.contains(&c.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_filters_cover_every_shape() {
        assert_eq!(size(">10mb"), Some((10 << 20, u64::MAX)));
        assert_eq!(size("<1gb"), Some((0, 1 << 30)));
        assert_eq!(size("=2mb"), Some((2 << 20, 2 << 20)));
        assert_eq!(size("500kb..2mb"), Some((500 << 10, 2 << 20)));
        assert_eq!(size("500kb-2mb"), Some((500 << 10, 2 << 20)));
        assert_eq!(
            size("1.5gb"),
            Some(((1.5 * (1u64 << 30) as f64) as u64, u64::MAX)),
            "a bare size means at least"
        );
        assert_eq!(size("10mb..1kb"), None, "inverted range");
        assert_eq!(size("bogus"), None);
    }

    #[test]
    fn size_constants_are_the_documented_bands() {
        assert_eq!(size("empty"), Some((0, 0)));
        assert_eq!(size("tiny"), Some((1, 10 << 10)));
        assert_eq!(size("GIGANTIC"), Some((128 * (1 << 20) + 1, u64::MAX)));
        // The bands abut without overlapping.
        let (_, tiny_hi) = size("tiny").unwrap();
        let (small_lo, _) = size("small").unwrap();
        assert_eq!(small_lo, tiny_hi + 1);
    }

    #[test]
    fn counts_read_as_numbers() {
        assert_eq!(count("5"), Some((5, 5)), "a bare count is exact");
        assert_eq!(count(">=2"), Some((2, u64::MAX)));
        assert_eq!(count("<3"), Some((0, 3)));
        assert_eq!(count("2..8"), Some((2, 8)));
        assert_eq!(count("8..2"), None);
        assert_eq!(count("many"), None);
        assert_eq!(limit("500"), Some(500));
    }

    #[test]
    fn absolute_dates_span_what_was_written() {
        let (lo, hi) = date("2024-06-15").unwrap();
        assert_eq!(hi - lo, DAY_NANOS - 1, "a bare date is one whole day");

        let (lo, hi) = date("2024-06").unwrap();
        assert_eq!(hi - lo, 30 * DAY_NANOS - 1, "June is 30 days");

        let (lo, hi) = date("2024").unwrap();
        assert!(hi - lo > 360 * DAY_NANOS, "a year");

        let (lo, hi) = date("2024-06-15T09:30").unwrap();
        assert_eq!(hi - lo, 60 * 1_000_000_000 - 1, "one minute");

        assert_eq!(date("20240615"), date("2024-06-15"), "compact form");
        assert!(date(">2024-01-01").unwrap().1 == i64::MAX);
        assert!(date("junk").is_none());
        assert!(date("2024-06-30..2024-01-01").is_none());
    }

    #[test]
    fn relative_dates_resolve_against_today() {
        let today = date("today").unwrap();
        let yesterday = date("yesterday").unwrap();
        assert_eq!(today.0 - yesterday.0, DAY_NANOS);

        // A named week covers seven days.
        let (lo, hi) = date("thisweek").unwrap();
        assert_eq!(hi - lo, 7 * DAY_NANOS - 1);
        assert!(date("lastweek").unwrap().1 < lo);

        // `last3days` ends today and covers three of them.
        let (lo, hi) = date("last3days").unwrap();
        assert_eq!(hi - lo, 3 * DAY_NANOS - 1);
        assert_eq!(hi, today.1);

        // Month and weekday names land on the most recent one.
        assert!(date("january").is_some());
        assert!(date("jan").is_some());
        assert!(date("monday").unwrap().1 <= today.1);
        assert!(date("mon").is_some());
        assert!(date("thismonth").is_some());
        assert!(date("nextyear").unwrap().0 > today.1);
    }

    #[test]
    fn attribute_letters_map_to_flags() {
        let (flags, unknown) = attributes("HS");
        assert!(flags.contains(EntryFlags::HIDDEN));
        assert!(flags.contains(EntryFlags::SYSTEM));
        assert!(unknown.is_empty());

        let (flags, unknown) = attributes("dR");
        assert!(flags.contains(EntryFlags::DIRECTORY));
        assert_eq!(unknown, vec!['R'], "read-only is not recorded");
        assert!(is_unrecorded_attribute('r'));
        assert!(!is_unrecorded_attribute('z'));
    }
}
