use anyhow::{Context, Result, anyhow};
use serenity::all::{CommandDataOptionValue, CommandInteraction};
use tracing::warn;

use crate::model::SurfaceFilter;
use crate::parsing::{parse_hhmm, parse_schedule};
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
        "subscribe" => {
            let day = get_string_opt(cmd, "day").context("missing 'day'")?;
            let from = get_string_opt(cmd, "from").context("missing 'from'")?;
            let to = get_string_opt(cmd, "to").context("missing 'to'")?;
            let schedule = parse_schedule(&day, today_berlin()).context("invalid 'day'")?;
            let start_minute = parse_hhmm(&from).context("invalid 'from' (expected HH:MM)")?;
            let end_minute = parse_hhmm(&to).context("invalid 'to' (expected HH:MM)")?;
            let courts = parse_courts(get_string_opt(cmd, "courts"))?;
            let surface = parse_surface(get_string_opt(cmd, "surface"))?;
            Ok(SubscriptionCommand::Subscribe {
                schedule,
                start_minute,
                end_minute,
                courts,
                surface,
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

fn parse_surface(input: Option<String>) -> Result<Option<SurfaceFilter>> {
    input
        .filter(|raw| !raw.trim().is_empty())
        .map(|raw| raw.parse().context("invalid 'surface'"))
        .transpose()
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
            Some(SurfaceFilter::CLAY)
        );
        assert_eq!(
            parse_surface(Some("all".into())).unwrap(),
            Some(SurfaceFilter::All)
        );
        assert!(parse_surface(Some("grass".into())).is_err());
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
}
