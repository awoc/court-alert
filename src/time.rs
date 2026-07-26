use std::fmt;

use chrono::{DateTime, Datelike, Days, NaiveDate, TimeZone, Timelike, Utc, Weekday};
use chrono_tz::Europe::Berlin;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocalSlotTime {
    pub(crate) date: NaiveDate,
    pub(crate) weekday: Weekday,
    pub(crate) minute_of_day: u32,
}

pub(crate) fn local_slot_time(dt: DateTime<Utc>) -> LocalSlotTime {
    let berlin = dt.with_timezone(&Berlin);
    LocalSlotTime {
        date: berlin.date_naive(),
        weekday: berlin.weekday(),
        minute_of_day: berlin.hour() * 60 + berlin.minute(),
    }
}

pub(crate) fn berlin_date(dt: DateTime<Utc>) -> NaiveDate {
    dt.with_timezone(&Berlin).date_naive()
}

pub(crate) fn today_berlin() -> NaiveDate {
    berlin_date(Utc::now())
}

pub(crate) fn utc_day_window(lookahead_days: i64) -> (DateTime<Utc>, DateTime<Utc>) {
    berlin_day_window_at(Utc::now(), lookahead_days)
}

fn berlin_day_window_at(now: DateTime<Utc>, lookahead_days: i64) -> (DateTime<Utc>, DateTime<Utc>) {
    let today = now.with_timezone(&Berlin).date_naive();
    let end_date = today
        .checked_add_days(Days::new(lookahead_days as u64))
        .expect("validated lookahead stays within chrono's date range");
    let start = Berlin
        .from_local_datetime(&today.and_hms_opt(0, 0, 0).expect("valid midnight"))
        .single()
        .expect("Berlin midnight is unambiguous")
        .with_timezone(&Utc);
    let end = Berlin
        .from_local_datetime(&end_date.and_hms_opt(0, 0, 0).expect("valid midnight"))
        .single()
        .expect("Berlin midnight is unambiguous")
        .with_timezone(&Utc);
    (start, end)
}

pub(crate) fn fmt_berlin_log(dt: DateTime<Utc>) -> String {
    dt.with_timezone(&Berlin)
        .format("%a %Y-%m-%d %H:%M %Z")
        .to_string()
}

pub(crate) fn fmt_berlin(dt: DateTime<Utc>) -> String {
    dt.with_timezone(&Berlin)
        .format("%a, %d.%m.%Y %H:%M")
        .to_string()
}

pub(crate) fn fmt_berlin_time(dt: DateTime<Utc>) -> String {
    dt.with_timezone(&Berlin).format("%H:%M").to_string()
}

pub(crate) fn fmt_hhmm(minute: u32) -> String {
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

/// Stamps log lines with Berlin wall-clock time, matching the timezone every
/// other rendered timestamp in the application uses.
pub struct BerlinTime;

impl FormatTime for BerlinTime {
    fn format_time(&self, writer: &mut Writer<'_>) -> fmt::Result {
        let now = Utc::now().with_timezone(&Berlin);
        write!(writer, "{}", now.format("%Y-%m-%d %H:%M:%S%.3f %Z"))
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    #[test]
    fn fmt_berlin_uses_short_weekday_and_dotted_date() {
        let dt = Utc.with_ymd_and_hms(2026, 6, 2, 18, 0, 0).unwrap();
        assert_eq!(fmt_berlin(dt), "Tue, 02.06.2026 20:00");
    }

    #[test]
    fn local_slot_time_projects_utc_to_berlin_business_time() {
        let dt = Utc.with_ymd_and_hms(2026, 6, 2, 18, 30, 0).unwrap();
        let local = local_slot_time(dt);
        assert_eq!(
            local.date,
            chrono::NaiveDate::from_ymd_opt(2026, 6, 2).unwrap()
        );
        assert_eq!(local.weekday, chrono::Weekday::Tue);
        assert_eq!(local.minute_of_day, 20 * 60 + 30);
    }

    #[test]
    fn day_window_uses_berlin_midnight_in_winter() {
        let now = Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
        let (start, end) = berlin_day_window_at(now, 7);
        assert_eq!(start, Utc.with_ymd_and_hms(2026, 1, 14, 23, 0, 0).unwrap());
        assert_eq!(end, Utc.with_ymd_and_hms(2026, 1, 21, 23, 0, 0).unwrap());
    }

    #[test]
    fn day_window_uses_berlin_midnight_in_summer() {
        let now = Utc.with_ymd_and_hms(2026, 6, 15, 12, 0, 0).unwrap();
        let (start, end) = berlin_day_window_at(now, 7);
        assert_eq!(start, Utc.with_ymd_and_hms(2026, 6, 14, 22, 0, 0).unwrap());
        assert_eq!(end, Utc.with_ymd_and_hms(2026, 6, 21, 22, 0, 0).unwrap());
    }

    #[test]
    fn day_window_tracks_dst_across_its_boundaries() {
        let now = Utc.with_ymd_and_hms(2026, 3, 27, 12, 0, 0).unwrap();
        let (start, end) = berlin_day_window_at(now, 7);
        assert_eq!(start, Utc.with_ymd_and_hms(2026, 3, 26, 23, 0, 0).unwrap());
        assert_eq!(end, Utc.with_ymd_and_hms(2026, 4, 2, 22, 0, 0).unwrap());
        assert_eq!(end - start, chrono::Duration::hours(167));
    }

    #[test]
    fn berlin_date_rolls_over_at_berlin_midnight_not_utc() {
        let dt = Utc.with_ymd_and_hms(2026, 6, 1, 23, 30, 0).unwrap();
        assert_eq!(
            berlin_date(dt),
            chrono::NaiveDate::from_ymd_opt(2026, 6, 2).unwrap()
        );
    }
}
