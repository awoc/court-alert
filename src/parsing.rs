use anyhow::{Context, Result, anyhow};
use chrono::{Datelike, NaiveDate, Weekday};

use crate::model::Schedule;

pub fn parse_schedule(s: &str, today: NaiveDate) -> Result<Schedule> {
    let s = s.trim();
    if let Ok(weekday) = s.parse::<Weekday>() {
        return Ok(Schedule::Weekday(weekday));
    }
    if s.contains('.') {
        return Ok(Schedule::Date(parse_date(s, today)?));
    }
    Err(anyhow!(
        "expected a weekday (e.g. Thu) or a date (e.g. 23.06.2026), got {s:?}"
    ))
}

pub fn parse_hhmm(s: &str) -> Result<u32> {
    let s = s.trim();
    let (h, m) = s
        .split_once(':')
        .ok_or_else(|| anyhow!("expected HH:MM, got {s:?}"))?;
    let h: u32 = h.parse().context("hour is not a number")?;
    let m: u32 = m.parse().context("minute is not a number")?;
    if h > 23 || m > 59 {
        return Err(anyhow!("expected hour 0-23 and minute 0-59"));
    }
    Ok(h * 60 + m)
}

pub fn parse_date(s: &str, today: NaiveDate) -> Result<NaiveDate> {
    let parts: Vec<&str> = s.split('.').map(str::trim).collect();
    if parts.len() != 2 && parts.len() != 3 {
        return Err(anyhow!("expected DD.MM.YYYY, got {s:?}"));
    }
    if parts.iter().any(|p| p.is_empty()) {
        return Err(anyhow!("expected DD.MM.YYYY, got {s:?}"));
    }
    let day: u32 = parts[0].parse().context("day is not a number")?;
    let month: u32 = parts[1].parse().context("month is not a number")?;
    match parts.get(2) {
        Some(y) => {
            let year: i32 = y.parse().context("year is not a number")?;
            let year = match y.len() {
                2 => 2000 + year,
                4 => year,
                _ => return Err(anyhow!("expected a two- or four-digit year, got {y:?}")),
            };
            NaiveDate::from_ymd_opt(year, month, day).ok_or_else(|| anyhow!("invalid date: {s}"))
        }
        None => (today.year()..=today.year().saturating_add(400))
            .filter_map(|y| NaiveDate::from_ymd_opt(y, month, day))
            .find(|d| *d >= today)
            .ok_or_else(|| anyhow!("invalid date: {s}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_hhmm() {
        assert_eq!(parse_hhmm("18:00").unwrap(), 18 * 60);
        assert_eq!(parse_hhmm("00:00").unwrap(), 0);
        assert_eq!(parse_hhmm("23:59").unwrap(), 23 * 60 + 59);
        assert_eq!(parse_hhmm("9:30").unwrap(), 9 * 60 + 30);
        assert_eq!(parse_hhmm(" 18:00 ").unwrap(), 18 * 60);
    }

    #[test]
    fn rejects_invalid_hhmm() {
        assert!(parse_hhmm("24:00").is_err());
        assert!(parse_hhmm("12:60").is_err());
        assert!(parse_hhmm("1800").is_err());
        assert!(parse_hhmm("ab:cd").is_err());
        assert!(parse_hhmm("").is_err());
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn parse_schedule_accepts_short_and_full_weekdays() {
        let today = d(2026, 1, 1);
        assert_eq!(
            parse_schedule("Thu", today).unwrap(),
            Schedule::Weekday(Weekday::Thu)
        );
        assert_eq!(
            parse_schedule("thursday", today).unwrap(),
            Schedule::Weekday(Weekday::Thu)
        );
        assert_eq!(
            parse_schedule(" tue ", today).unwrap(),
            Schedule::Weekday(Weekday::Tue)
        );
    }

    #[test]
    fn parse_schedule_treats_dotted_input_as_date() {
        let today = d(2026, 1, 1);
        assert_eq!(
            parse_schedule("23.06.2026", today).unwrap(),
            Schedule::Date(d(2026, 6, 23))
        );
        assert_eq!(
            parse_schedule("23.6", today).unwrap(),
            Schedule::Date(d(2026, 6, 23))
        );
        assert!(
            parse_schedule("32.13.2026", today)
                .unwrap_err()
                .to_string()
                .contains("date")
        );
    }

    #[test]
    fn parse_schedule_rejects_gibberish() {
        assert!(parse_schedule("xxx", d(2026, 1, 1)).is_err());
        assert!(parse_schedule("", d(2026, 1, 1)).is_err());
    }

    #[test]
    fn parse_date_with_explicit_year() {
        assert_eq!(
            parse_date("23.06.2026", d(2025, 1, 1)).unwrap(),
            d(2026, 6, 23)
        );
    }

    #[test]
    fn parse_date_expands_two_digit_year() {
        assert_eq!(
            parse_date("23.6.26", d(2025, 1, 1)).unwrap(),
            d(2026, 6, 23)
        );
    }

    #[test]
    fn parse_date_yearless_uses_current_year_when_not_past() {
        assert_eq!(parse_date("23.6", d(2026, 1, 1)).unwrap(), d(2026, 6, 23));
    }

    #[test]
    fn parse_date_yearless_today_stays_today() {
        assert_eq!(
            parse_date("28.12", d(2026, 12, 28)).unwrap(),
            d(2026, 12, 28)
        );
    }

    #[test]
    fn parse_date_yearless_rolls_over_new_year() {
        assert_eq!(parse_date("5.1", d(2026, 12, 28)).unwrap(), d(2027, 1, 5));
    }

    #[test]
    fn parse_date_yearless_leap_day_rolls_to_leap_year() {
        assert_eq!(parse_date("29.2", d(2027, 3, 1)).unwrap(), d(2028, 2, 29));
    }

    #[test]
    fn parse_date_yearless_leap_day_after_leap_day_finds_next_cycle() {
        assert_eq!(parse_date("29.2", d(2028, 3, 1)).unwrap(), d(2032, 2, 29));
    }

    #[test]
    fn parse_date_rejects_odd_year_widths() {
        let today = d(2026, 1, 1);
        assert!(parse_date("23.6.026", today).is_err()); // 3 digits
        assert!(parse_date("23.6.20260", today).is_err()); // 5 digits
        assert!(parse_date("23.6.6", today).is_err()); // 1 digit
    }

    #[test]
    fn parse_date_rejects_malformed() {
        let today = d(2026, 1, 1);
        assert!(parse_date("2026-06-23", today).is_err()); // ISO, wrong separator
        assert!(parse_date("foo", today).is_err());
        assert!(parse_date("32.13.2026", today).is_err()); // out of range
        assert!(parse_date("23", today).is_err()); // no month
        assert!(parse_date("", today).is_err());
        assert!(parse_date("23.6.", today).is_err()); // trailing dot
        assert!(parse_date("23..2026", today).is_err()); // empty month
    }
}
