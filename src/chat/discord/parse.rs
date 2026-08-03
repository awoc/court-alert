use anyhow::{Context, Result, anyhow};
use chrono::{Datelike, NaiveDate, Weekday};
use serenity::all::{CommandDataOptionValue, CommandInteraction};
use tracing::warn;

use crate::model::{CourtFilter, Schedule, Sport, VenueId};
use crate::subscriptions::contract::SubscriptionCommand;
use crate::time::today_berlin;

const MAX_COURTS: usize = 20;
const MAX_COURT_NAME_CHARS: usize = 100;
const MAX_COURT_FILTER_CHARS: usize = 500;

const UNSUBSCRIBE_PREFIX: &str = "unsubscribe:";

pub(super) fn unsubscribe_custom_id(id: i64) -> String {
    format!("{UNSUBSCRIBE_PREFIX}{id}")
}

pub(super) fn parse_component(custom_id: &str) -> Result<SubscriptionCommand> {
    let id = custom_id
        .strip_prefix(UNSUBSCRIBE_PREFIX)
        .and_then(|id| id.parse().ok())
        .with_context(|| format!("unknown component id: {custom_id:?}"))?;
    Ok(SubscriptionCommand::Unsubscribe { id })
}

pub(super) fn parse_interaction(cmd: &CommandInteraction) -> Result<SubscriptionCommand> {
    match cmd.data.name.as_str() {
        "tennis" => {
            let (schedule, start_minute, end_minute) = parse_when(cmd)?;
            Ok(SubscriptionCommand::Subscribe {
                sport: Sport::Tennis,
                venue: None,
                schedule,
                start_minute,
                end_minute,
                courts: parse_courts(get_string_opt(cmd, "courts"))?,
                filter: parse_surface(get_string_opt(cmd, "surface"))?,
            })
        }
        "padel" => {
            let (schedule, start_minute, end_minute) = parse_when(cmd)?;
            Ok(SubscriptionCommand::Subscribe {
                sport: Sport::Padel,
                venue: get_string_opt(cmd, "club")
                    .filter(|raw| !raw.trim().is_empty())
                    .map(|raw| VenueId::new(raw.trim())),
                schedule,
                start_minute,
                end_minute,
                courts: None,
                filter: parse_location(get_string_opt(cmd, "location"))?,
            })
        }
        "list" => Ok(SubscriptionCommand::List),
        "listall" => Ok(SubscriptionCommand::ListAll),
        "unsubscribe" => {
            let id = get_integer_opt(cmd, "id").context("missing 'id'")?;
            Ok(SubscriptionCommand::Unsubscribe { id })
        }
        other => {
            warn!(name = %other, "unknown slash command");
            Err(anyhow!("unknown command: {other}"))
        }
    }
}

fn parse_when(cmd: &CommandInteraction) -> Result<(Schedule, u32, u32)> {
    let day = get_string_opt(cmd, "day").context("missing 'day'")?;
    let from = get_string_opt(cmd, "from").context("missing 'from'")?;
    let to = get_string_opt(cmd, "to").context("missing 'to'")?;
    Ok((
        parse_schedule(&day, today_berlin()).context("invalid 'day'")?,
        parse_hhmm(&from).context("invalid 'from' (expected HH:MM)")?,
        parse_hhmm(&to).context("invalid 'to' (expected HH:MM)")?,
    ))
}

fn parse_courts(input: Option<String>) -> Result<Option<Vec<String>>> {
    let Some(input) = input else {
        return Ok(None);
    };
    anyhow::ensure!(
        input.chars().count() <= MAX_COURT_FILTER_CHARS,
        "'courts' must be at most {MAX_COURT_FILTER_CHARS} characters"
    );
    let courts: Vec<String> = input
        .split(',')
        .map(str::trim)
        .filter(|court| !court.is_empty())
        .map(str::to_string)
        .collect();
    anyhow::ensure!(
        courts.len() <= MAX_COURTS,
        "'courts' must contain at most {MAX_COURTS} names"
    );
    anyhow::ensure!(
        courts
            .iter()
            .all(|court| court.chars().count() <= MAX_COURT_NAME_CHARS),
        "each court name must be at most {MAX_COURT_NAME_CHARS} characters"
    );
    Ok((!courts.is_empty()).then_some(courts))
}

fn parse_surface(input: Option<String>) -> Result<Option<CourtFilter>> {
    parse_filter(input, "surface", |filter| {
        matches!(filter, CourtFilter::Any | CourtFilter::Surface(_))
    })
}

fn parse_location(input: Option<String>) -> Result<Option<CourtFilter>> {
    parse_filter(input, "location", |filter| {
        matches!(filter, CourtFilter::Any | CourtFilter::Location(_))
    })
}

fn parse_filter(
    input: Option<String>,
    option: &str,
    accepted: impl Fn(CourtFilter) -> bool,
) -> Result<Option<CourtFilter>> {
    let Some(raw) = input.filter(|raw| !raw.trim().is_empty()) else {
        return Ok(None);
    };
    let filter: CourtFilter = raw.parse().with_context(|| format!("invalid '{option}'"))?;
    anyhow::ensure!(accepted(filter), "'{option}' does not accept {filter:?}");
    Ok(Some(filter))
}

fn get_string_opt(cmd: &CommandInteraction, name: &str) -> Option<String> {
    cmd.data
        .options
        .iter()
        .find(|o| o.name == name)
        .and_then(|o| match &o.value {
            CommandDataOptionValue::String(s) => Some(s.clone()),
            _ => None,
        })
}

fn get_integer_opt(cmd: &CommandInteraction, name: &str) -> Option<i64> {
    cmd.data
        .options
        .iter()
        .find(|o| o.name == name)
        .and_then(|o| match &o.value {
            CommandDataOptionValue::Integer(n) => Some(*n),
            _ => None,
        })
}

pub(super) fn parse_schedule(s: &str, today: NaiveDate) -> Result<Schedule> {
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

pub(super) fn parse_hhmm(s: &str) -> Result<u32> {
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

pub(super) fn parse_date(s: &str, today: NaiveDate) -> Result<NaiveDate> {
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
    fn an_unsubscribe_button_click_round_trips_its_reminder_id() {
        let command = parse_component(&unsubscribe_custom_id(7)).unwrap();
        assert!(matches!(
            command,
            SubscriptionCommand::Unsubscribe { id: 7 }
        ));
    }

    #[test]
    fn unknown_component_ids_are_rejected() {
        for custom_id in ["", "unsubscribe:", "unsubscribe:abc", "resubscribe:7", "7"] {
            assert!(
                parse_component(custom_id).is_err(),
                "expected {custom_id:?} to be rejected"
            );
        }
    }

    #[test]
    fn court_filter_is_trimmed_and_empty_names_are_ignored() {
        assert_eq!(
            parse_courts(Some(" Court 1, ,Court 2 ".into())).unwrap(),
            Some(vec!["Court 1".into(), "Court 2".into()])
        );
        assert_eq!(parse_courts(Some(" , ".into())).unwrap(), None);
    }

    #[test]
    fn surface_is_optional_and_validated() {
        assert_eq!(parse_surface(None).unwrap(), None);
        assert_eq!(parse_surface(Some("  ".into())).unwrap(), None);
        assert_eq!(
            parse_surface(Some("clay".into())).unwrap(),
            Some(CourtFilter::CLAY)
        );
        assert_eq!(
            parse_surface(Some("all".into())).unwrap(),
            Some(CourtFilter::Any)
        );
        assert!(parse_surface(Some("grass".into())).is_err());
    }

    #[test]
    fn an_option_rejects_the_other_sports_vocabulary() {
        for raw in ["indoor", "outdoor"] {
            assert!(
                parse_surface(Some(raw.into())).is_err(),
                "surface accepted {raw:?}"
            );
        }
        for raw in ["clay", "synthetic"] {
            assert!(
                parse_location(Some(raw.into())).is_err(),
                "location accepted {raw:?}"
            );
        }
    }

    #[test]
    fn location_is_optional_and_validated() {
        assert_eq!(parse_location(None).unwrap(), None);
        assert_eq!(
            parse_location(Some("indoor".into())).unwrap(),
            Some(CourtFilter::Location(crate::model::CourtLocation::Indoor))
        );
        assert_eq!(
            parse_location(Some("any".into())).unwrap(),
            Some(CourtFilter::Any)
        );
        assert!(parse_location(Some("grass".into())).is_err());
    }

    #[test]
    fn court_filter_rejects_oversized_input() {
        assert!(parse_courts(Some("x".repeat(MAX_COURT_FILTER_CHARS + 1))).is_err());
        let too_many = (0..=MAX_COURTS)
            .map(|i| format!("Court {i}"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(parse_courts(Some(too_many)).is_err());
        assert!(parse_courts(Some("x".repeat(MAX_COURT_NAME_CHARS + 1))).is_err());
    }

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
        assert!(parse_date("23.6.026", today).is_err());
        assert!(parse_date("23.6.20260", today).is_err());
        assert!(parse_date("23.6.6", today).is_err());
    }

    #[test]
    fn parse_date_rejects_malformed() {
        let today = d(2026, 1, 1);
        assert!(parse_date("2026-06-23", today).is_err());
        assert!(parse_date("foo", today).is_err());
        assert!(parse_date("32.13.2026", today).is_err());
        assert!(parse_date("23", today).is_err());
        assert!(parse_date("", today).is_err());
        assert!(parse_date("23.6.", today).is_err());
        assert!(parse_date("23..2026", today).is_err());
    }
}
