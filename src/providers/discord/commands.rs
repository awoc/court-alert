use anyhow::{Context as _, Result};
use serenity::all::{
    Command as SerenityCommand, CommandOptionType, Context, CreateCommand, CreateCommandOption,
    GuildId,
};
use tracing::info;

pub(super) async fn register_commands(ctx: &Context, guild_id: Option<GuildId>) -> Result<()> {
    let cmds = vec![
        build_subscribe_cmd(),
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

fn build_subscribe_cmd() -> CreateCommand {
    CreateCommand::new("subscribe")
        .description("Get a DM when courts become free in a time window")
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
                "From (HH:MM, Berlin time)",
            )
            .max_length(5)
            .required(true),
        )
        .add_option(
            CreateCommandOption::new(CommandOptionType::String, "to", "To (HH:MM, Berlin time)")
                .max_length(5)
                .required(true),
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
