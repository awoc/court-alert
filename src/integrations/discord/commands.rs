use anyhow::{Context as _, Result};
use serenity::all::{
    Command as SerenityCommand, CommandOptionType, Context, CreateCommand, CreateCommandOption,
    GuildId,
};
use tracing::info;

use crate::model::VenueId;

use super::MAX_CLUB_CHOICES;

pub(super) async fn register_commands(
    ctx: &Context,
    guild_id: Option<GuildId>,
    padel_clubs: &[(VenueId, String)],
) -> Result<()> {
    let cmds = vec![
        build_tennis_cmd(),
        build_padel_cmd(padel_clubs),
        build_list_cmd(),
        build_unsubscribe_cmd(),
        build_listall_cmd(),
        build_help_cmd(),
    ];
    SerenityCommand::set_global_commands(&ctx.http, cmds.clone())
        .await
        .context("registering global commands")?;
    info!("registered global slash commands (DM-capable; propagation can take up to 1h)");
    if let Some(gid) = guild_id {
        gid.set_commands(&ctx.http, cmds)
            .await
            .context("registering guild commands")?;
        info!(guild = %gid, "also registered guild slash commands (instant, dev fast-path)");
    }
    Ok(())
}

fn with_day_from_to(command: CreateCommand) -> CreateCommand {
    command
        .add_option(
            CreateCommandOption::new(
                CommandOptionType::String,
                "day",
                "Weekday (e.g. Thu) for every week, or a date (e.g. 23.06.2026) for one day",
            )
            .max_length(10)
            .required(true),
        )
        .add_option(
            CreateCommandOption::new(
                CommandOptionType::String,
                "from",
                "From (HH:MM or whole hour, Berlin time)",
            )
            .max_length(5)
            .required(true),
        )
        .add_option(
            CreateCommandOption::new(
                CommandOptionType::String,
                "to",
                "To (HH:MM or whole hour, Berlin time)",
            )
            .max_length(5)
            .required(true),
        )
}

fn build_tennis_cmd() -> CreateCommand {
    with_day_from_to(
        CreateCommand::new("tennis")
            .description("Get a DM when tennis courts become free in a time window"),
    )
    .add_option(
        CreateCommandOption::new(
            CommandOptionType::String,
            "courts",
            "Court numbers (comma-separated, e.g. 2, 19)",
        )
        .max_length(500)
        .required(false),
    )
    .add_option(
        CreateCommandOption::new(
            CommandOptionType::String,
            "surface",
            "Surface to watch; defaults to clay",
        )
        .add_string_choice("Clay", "clay")
        .add_string_choice("Synthetic", "synthetic")
        .add_string_choice("All", "all")
        .required(false),
    )
}

fn build_padel_cmd(clubs: &[(VenueId, String)]) -> CreateCommand {
    let command = with_day_from_to(
        CreateCommand::new("padel")
            .description("Get a DM when padel courts become free in a time window"),
    );

    let command = if clubs.is_empty() {
        command
    } else {
        let mut club = CreateCommandOption::new(
            CommandOptionType::String,
            "club",
            "Club to watch; defaults to all clubs",
        )
        .required(false);
        for (id, display_name) in clubs.iter().take(MAX_CLUB_CHOICES) {
            club = club.add_string_choice(display_name, id.as_str());
        }
        command.add_option(club)
    };

    command.add_option(
        CreateCommandOption::new(
            CommandOptionType::String,
            "location",
            "Indoor or outdoor; defaults to any",
        )
        .add_string_choice("Indoor", "indoor")
        .add_string_choice("Outdoor", "outdoor")
        .add_string_choice("Any", "any")
        .required(false),
    )
}

fn build_list_cmd() -> CreateCommand {
    CreateCommand::new("list").description("Show your reminders")
}

fn build_listall_cmd() -> CreateCommand {
    CreateCommand::new("listall").description("Show all reminders (admin only)")
}

fn build_unsubscribe_cmd() -> CreateCommand {
    CreateCommand::new("unsubscribe")
        .description("Delete a reminder")
        .add_option(
            CreateCommandOption::new(CommandOptionType::Integer, "id", "Reminder ID")
                .required(true)
                .min_int_value(1),
        )
}

fn build_help_cmd() -> CreateCommand {
    CreateCommand::new("help").description("Explain the available commands")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clubs(count: usize) -> Vec<(VenueId, String)> {
        (0..count)
            .map(|i| (VenueId::new(format!("club-{i}")), format!("Club {i}")))
            .collect()
    }

    fn option_names(command: &CreateCommand) -> Vec<String> {
        let json = serde_json::to_value(command).unwrap();
        json["options"]
            .as_array()
            .map(|options| {
                options
                    .iter()
                    .map(|option| option["name"].as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn choice_values(command: &CreateCommand, option: &str) -> Vec<String> {
        let json = serde_json::to_value(command).unwrap();
        json["options"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["name"] == option)
            .and_then(|o| o["choices"].as_array())
            .map(|choices| {
                choices
                    .iter()
                    .map(|choice| choice["value"].as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn padel_offers_the_configured_clubs_as_choices() {
        let command = build_padel_cmd(&clubs(2));

        assert_eq!(
            choice_values(&command, "club"),
            vec!["club-0".to_string(), "club-1".to_string()]
        );
    }

    #[test]
    fn padel_omits_the_club_option_when_no_club_is_configured() {
        let names = option_names(&build_padel_cmd(&[]));

        assert!(!names.contains(&"club".to_string()), "got {names:?}");
        assert!(names.contains(&"location".to_string()));
    }

    #[test]
    fn padel_never_exceeds_discords_choice_limit() {
        let command = build_padel_cmd(&clubs(MAX_CLUB_CHOICES + 5));

        assert_eq!(choice_values(&command, "club").len(), MAX_CLUB_CHOICES);
    }

    #[test]
    fn every_club_of_a_supported_configuration_is_offered_by_name() {
        let command = build_padel_cmd(&clubs(MAX_CLUB_CHOICES));

        assert_eq!(choice_values(&command, "club").len(), MAX_CLUB_CHOICES);
    }

    #[test]
    fn padel_has_no_courts_option() {
        assert!(!option_names(&build_padel_cmd(&clubs(1))).contains(&"courts".to_string()));
        assert!(option_names(&build_tennis_cmd()).contains(&"courts".to_string()));
    }

    #[test]
    fn tennis_command_is_named_tennis() {
        let json = serde_json::to_value(build_tennis_cmd()).unwrap();

        assert_eq!(json["name"], "tennis");
    }

    #[test]
    fn padel_offers_indoor_outdoor_and_any() {
        assert_eq!(
            choice_values(&build_padel_cmd(&clubs(1)), "location"),
            vec![
                "indoor".to_string(),
                "outdoor".to_string(),
                "any".to_string()
            ]
        );
    }
}
