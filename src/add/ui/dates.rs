//! Smart date resolution, against a fixed "today".
//!
//! Accepted forms: full `YYYY-MM-DD` (separators `-`, `/`, `.`), partial
//! `M-D` (current year), bare `D` (current month), and the words `today`,
//! `yesterday`, `tomorrow`. Empty input means today. The resolved date is
//! rendered in the live block as the user types, so a misunderstood date is
//! visible before it is committed.

use chrono::{Datelike, Days, NaiveDate};

/// Resolve a typed date against `today`. `None` means not (yet) a date.
#[must_use]
pub fn resolve(input: &str, today: NaiveDate) -> Option<NaiveDate> {
    let s = input.trim();
    if s.is_empty() {
        return Some(today);
    }
    match s.to_ascii_lowercase().as_str() {
        "today" => return Some(today),
        "yesterday" => return today.checked_sub_days(Days::new(1)),
        "tomorrow" => return today.checked_add_days(Days::new(1)),
        _ => {}
    }
    let parts: Vec<&str> = s.split(['-', '/', '.']).collect();
    let nums: Option<Vec<u32>> = parts.iter().map(|p| p.parse::<u32>().ok()).collect();
    let nums = nums?;
    match nums.as_slice() {
        [d] => NaiveDate::from_ymd_opt(today.year(), today.month(), *d),
        [m, d] => NaiveDate::from_ymd_opt(today.year(), *m, *d),
        [y, m, d] => NaiveDate::from_ymd_opt(i32::try_from(*y).ok()?, *m, *d),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    const TODAY: fn() -> NaiveDate = || d(2026, 8, 10);

    #[test]
    fn empty_input_is_today() {
        assert_eq!(resolve("", TODAY()), Some(TODAY()));
        assert_eq!(resolve("  ", TODAY()), Some(TODAY()));
    }

    #[test]
    fn words() {
        assert_eq!(resolve("today", TODAY()), Some(d(2026, 8, 10)));
        assert_eq!(resolve("yesterday", TODAY()), Some(d(2026, 8, 9)));
        assert_eq!(resolve("tomorrow", TODAY()), Some(d(2026, 8, 11)));
        assert_eq!(resolve("Yesterday", TODAY()), Some(d(2026, 8, 9)));
    }

    #[test]
    fn bare_day_is_this_month() {
        assert_eq!(resolve("30", TODAY()), Some(d(2026, 8, 30)));
        assert_eq!(resolve("1", TODAY()), Some(d(2026, 8, 1)));
    }

    #[test]
    fn month_day_is_this_year() {
        assert_eq!(resolve("8/30", TODAY()), Some(d(2026, 8, 30)));
        assert_eq!(resolve("2-14", TODAY()), Some(d(2026, 2, 14)));
        assert_eq!(resolve("2.14", TODAY()), Some(d(2026, 2, 14)));
    }

    #[test]
    fn full_dates() {
        assert_eq!(resolve("2025-12-31", TODAY()), Some(d(2025, 12, 31)));
        assert_eq!(resolve("2025/12/31", TODAY()), Some(d(2025, 12, 31)));
    }

    #[test]
    fn invalid_dates_are_none() {
        assert_eq!(resolve("32", TODAY()), None);
        assert_eq!(resolve("13-40", TODAY()), None);
        assert_eq!(resolve("banana", TODAY()), None);
        assert_eq!(resolve("2026-02-30", TODAY()), None);
    }
}
