use std::future::Future;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use serenity::all::{
    ButtonStyle, CommandInteraction, ComponentInteraction, Context, CreateActionRow, CreateButton,
    CreateInteractionResponse, CreateInteractionResponseFollowup, CreateInteractionResponseMessage,
    EventHandler, GuildId, Interaction, Message, Ready, UserId,
};
use serenity::async_trait;
use tracing::{error, info, warn};

use crate::model::ProviderUserRef;
use crate::providers::ReadySignal;
use crate::subscriptions::SubscriptionService;
use crate::subscriptions::contract::SubscriptionCommand;

use super::PROVIDER_NAME;
use super::commands::register_commands;
use super::parse::{parse_component, parse_interaction, unsubscribe_custom_id};
use super::render::{ReplyMessage, render_help, render_reply, render_text};

pub(super) struct Handler {
    pub(super) service: Arc<SubscriptionService>,
    pub(super) guild_id: Option<GuildId>,
    pub(super) ready: ReadySignal,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!(user = %ready.user.name, "discord provider connected");
        self.ready.ready();
        if let Err(e) = register_commands(&ctx, self.guild_id).await {
            error!(error = %format!("{e:#}"), "failed to register slash commands");
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(cmd) => self.handle_command(&ctx, &cmd).await,
            Interaction::Component(component) => self.handle_component(&ctx, &component).await,
            _ => {}
        }
    }
}

impl Handler {
    async fn handle_command(&self, ctx: &Context, cmd: &CommandInteraction) {
        let messages = if cmd.data.name == "help" {
            render_help()
        } else {
            match parse_interaction(cmd) {
                Ok(command) => self.run(cmd.user.id, command).await,
                Err(e) => render_text(&format!("Error: {e:#}")),
            }
        };
        if let Err(e) = reply(ctx, cmd, &messages).await {
            error!(error = %format!("{e:#}"), "failed to send interaction response");
        }
    }

    async fn handle_component(&self, ctx: &Context, component: &ComponentInteraction) {
        let command = match parse_component(&component.data.custom_id) {
            Ok(command) => command,
            Err(e) => {
                warn!(error = %format!("{e:#}"), "unknown component interaction");
                // Every interaction needs an answer within 3s, or the click ends
                // in a red "This interaction failed".
                if let Err(e) = answer_stale_button(ctx, component).await {
                    error!(error = %format!("{e:#}"), "failed to answer unknown component");
                }
                return;
            }
        };
        let messages = self.run(component.user.id, command).await;
        if let Err(e) = replace_message(ctx, component, &messages).await {
            error!(error = %format!("{e:#}"), "failed to update interaction message");
        }
    }

    async fn run(&self, user_id: UserId, command: SubscriptionCommand) -> Vec<ReplyMessage> {
        let user = ProviderUserRef {
            provider: PROVIDER_NAME.to_string(),
            user_id: user_id.get().to_string(),
        };
        match self.service.handle(&user, command).await {
            Ok(reply) => render_reply(&reply),
            Err(e) => {
                error!(error = %format!("{e:#}"), "command handling failed");
                render_text("Error: something went wrong internally, please try again later.")
            }
        }
    }
}

async fn reply(ctx: &Context, cmd: &CommandInteraction, messages: &[ReplyMessage]) -> Result<()> {
    let ephemeral = hides_from_others(cmd.guild_id);
    let first = messages
        .first()
        .context("cannot send an empty interaction response")?;
    let response = CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .content(&first.content)
            .components(components(first))
            .ephemeral(ephemeral),
    );
    cmd.create_response(&ctx.http, response)
        .await
        .context("sending interaction response")?;
    send_followups(&messages[1..], ephemeral, |f| {
        cmd.create_followup(&ctx.http, f)
    })
    .await;
    Ok(())
}

async fn replace_message(
    ctx: &Context,
    component: &ComponentInteraction,
    messages: &[ReplyMessage],
) -> Result<()> {
    let first = messages
        .first()
        .context("cannot send an empty interaction response")?;
    let response = CreateInteractionResponse::UpdateMessage(
        CreateInteractionResponseMessage::new()
            .content(&first.content)
            .components(components(first)),
    );
    component
        .create_response(&ctx.http, response)
        .await
        .context("updating the message the component belongs to")?;
    let ephemeral = hides_from_others(component.guild_id);
    send_followups(&messages[1..], ephemeral, |f| {
        component.create_followup(&ctx.http, f)
    })
    .await;
    Ok(())
}

/// A button whose id we no longer understand — after a rename of the wire
/// format, say. Answering keeps the click from failing visibly.
async fn answer_stale_button(ctx: &Context, component: &ComponentInteraction) -> Result<()> {
    let response = CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .content("This button no longer works. Run `/list` to get a fresh one.")
            .ephemeral(hides_from_others(component.guild_id)),
    );
    component
        .create_response(&ctx.http, response)
        .await
        .context("answering an unknown component interaction")
}

/// A follow-up that fails must not drop the ones behind it: a half-sent list
/// looks like a complete one, so the user is told what is missing instead.
async fn send_followups<S, F>(messages: &[ReplyMessage], ephemeral: bool, send: S)
where
    S: Fn(CreateInteractionResponseFollowup) -> F,
    F: Future<Output = serenity::Result<Message>>,
{
    let mut failed = 0;
    for message in messages {
        if let Err(e) = send(followup(message, ephemeral)).await {
            error!(error = %format!("{e:#}"), "failed to send interaction response follow-up");
            failed += 1;
        }
    }
    if failed == 0 {
        return;
    }
    let note = render_text(&format!(
        "⚠️ {failed} message(s) of this reply could not be delivered. Please try again."
    ));
    for message in &note {
        if let Err(e) = send(followup(message, ephemeral)).await {
            error!(error = %format!("{e:#}"), "failed to report undelivered follow-ups");
        }
    }
}

fn followup(message: &ReplyMessage, ephemeral: bool) -> CreateInteractionResponseFollowup {
    CreateInteractionResponseFollowup::new()
        .content(&message.content)
        .components(components(message))
        .ephemeral(ephemeral)
}

fn hides_from_others(guild_id: Option<GuildId>) -> bool {
    guild_id.is_some()
}

fn components(message: &ReplyMessage) -> Vec<CreateActionRow> {
    message.unsubscribe_id.map_or_else(Vec::new, |id| {
        vec![CreateActionRow::Buttons(vec![
            CreateButton::new(unsubscribe_custom_id(id))
                .label("Unsubscribe")
                .style(ButtonStyle::Danger),
        ])]
    })
}
