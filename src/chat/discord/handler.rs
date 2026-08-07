use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context as _, Result};
use serenity::all::{
    ButtonStyle, CommandInteraction, ComponentInteraction, Context, CreateActionRow, CreateButton,
    CreateInteractionResponse, CreateInteractionResponseFollowup, CreateInteractionResponseMessage,
    EditInteractionResponse, EventHandler, GuildId, Interaction, InteractionContext, Message,
    Ready, UserId,
};
use serenity::async_trait;
use tracing::{error, info, warn};

use crate::chat::ReadySignal;
use crate::model::ProviderUserRef;
use crate::model::Sport;
use crate::subscriptions::SubscriptionService;
use crate::subscriptions::contract::{SubscriptionCommand, SubscriptionResult};

use super::PROVIDER_NAME;
use super::commands::register_commands;
use super::parse::{parse_component, parse_interaction, unsubscribe_custom_id};
use super::render::{ReplyMessage, render_button_reply, render_help, render_reply, render_text};

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
        let padel_clubs = self.service.clubs_of(Sport::Padel);
        if let Err(e) = register_commands(&ctx, self.guild_id, &padel_clubs).await {
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
        let started = Instant::now();
        info!(command = %cmd.data.name, user = %cmd.user.id, "interaction received");
        // Answering takes a database round trip the 3s interaction deadline
        // cannot be relied on to cover; deferring first buys 15 minutes.
        let ephemeral = hides_from_others(cmd.context);
        if let Err(e) = defer(ctx, cmd, ephemeral).await {
            error!(error = %format!("{e:#}"), "failed to acknowledge interaction");
            return;
        }
        let messages = if cmd.data.name == "help" {
            render_help()
        } else {
            match parse_interaction(cmd) {
                Ok(command) => self.run(cmd.user.id, command, render_reply).await,
                Err(e) => render_text(&format!("Error: {e:#}")),
            }
        };
        if let Err(e) = reply(ctx, cmd, &messages, ephemeral).await {
            error!(error = %format!("{e:#}"), "failed to send interaction response");
            return;
        }
        info!(
            command = %cmd.data.name,
            elapsed_ms = started.elapsed().as_millis(),
            "interaction answered"
        );
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
        let messages = self
            .run(component.user.id, command, render_button_reply)
            .await;
        if let Err(e) = replace_message(ctx, component, &messages).await {
            error!(error = %format!("{e:#}"), "failed to update interaction message");
        }
    }

    async fn run(
        &self,
        user_id: UserId,
        command: SubscriptionCommand,
        render: impl Fn(&SubscriptionResult) -> Vec<ReplyMessage> + Send,
    ) -> Vec<ReplyMessage> {
        let user = ProviderUserRef {
            provider: PROVIDER_NAME.to_string(),
            user_id: user_id.get().to_string(),
        };
        match self.service.handle(&user, command).await {
            Ok(reply) => render(&reply),
            Err(e) => {
                error!(error = %format!("{e:#}"), "command handling failed");
                render_text("Error: something went wrong internally, please try again later.")
            }
        }
    }
}

async fn defer(ctx: &Context, cmd: &CommandInteraction, ephemeral: bool) -> Result<()> {
    let response = CreateInteractionResponse::Defer(
        CreateInteractionResponseMessage::new().ephemeral(ephemeral),
    );
    cmd.create_response(&ctx.http, response)
        .await
        .context("deferring the interaction response")
}

async fn reply(
    ctx: &Context,
    cmd: &CommandInteraction,
    messages: &[ReplyMessage],
    ephemeral: bool,
) -> Result<()> {
    let first = messages
        .first()
        .context("cannot send an empty interaction response")?;
    cmd.edit_response(
        &ctx.http,
        EditInteractionResponse::new()
            .content(&first.content)
            .components(components(first)),
    )
    .await
    .context("sending the deferred interaction response")?;
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
    let ephemeral = hides_from_others(component.context);
    send_followups(&messages[1..], ephemeral, |f| {
        component.create_followup(&ctx.http, f)
    })
    .await;
    Ok(())
}

/// A button whose id we no longer understand — after a rename of the wire
/// format, say. Replacing the message it sits on both answers the click and
/// retires the button, so it cannot be clicked into the same dead end again.
async fn answer_stale_button(ctx: &Context, component: &ComponentInteraction) -> Result<()> {
    let response = CreateInteractionResponse::UpdateMessage(
        CreateInteractionResponseMessage::new()
            .content("This button no longer works. Run `/list` to get a fresh one.")
            .components(Vec::new()),
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

fn hides_from_others(context: Option<InteractionContext>) -> bool {
    context != Some(InteractionContext::BotDm)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_dm_with_the_bot_is_private_enough_to_show_a_reply_openly() {
        assert!(!hides_from_others(Some(InteractionContext::BotDm)));
    }

    #[test]
    fn replies_anywhere_others_can_read_along_stay_hidden() {
        for context in [
            InteractionContext::Guild,
            InteractionContext::PrivateChannel,
        ] {
            assert!(
                hides_from_others(Some(context)),
                "{context:?} must not be answered in the open"
            );
        }
    }

    #[test]
    fn a_missing_context_is_treated_as_public() {
        assert!(hides_from_others(None));
    }

    #[test]
    fn a_message_without_a_reminder_id_carries_no_button() {
        let message = ReplyMessage {
            content: "**Your reminders:**".to_string(),
            unsubscribe_id: None,
        };
        assert!(components(&message).is_empty());
    }

    #[test]
    fn a_listed_reminder_carries_a_button_naming_it() {
        let message = ReplyMessage {
            content: "#7 – Tue".to_string(),
            unsubscribe_id: Some(7),
        };
        let rows = components(&message);
        assert_eq!(rows.len(), 1);
        // The id has to survive the round trip, or the click cannot be routed.
        let command = parse_component(&unsubscribe_custom_id(7)).unwrap();
        assert!(matches!(
            command,
            SubscriptionCommand::Unsubscribe { id: 7 }
        ));
    }
}
