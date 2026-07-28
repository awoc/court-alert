use std::sync::Arc;

use anyhow::{Context as _, Result};
use serenity::all::{
    CommandInteraction, Context, CreateInteractionResponse, CreateInteractionResponseFollowup,
    CreateInteractionResponseMessage, EventHandler, GuildId, Interaction, Ready,
};
use serenity::async_trait;
use tracing::{error, info};

use crate::model::ProviderUserRef;
use crate::providers::ReadySignal;
use crate::subscriptions::SubscriptionService;
use crate::text::{DISCORD_CHUNK_BUDGET, chunk_lines};

use super::PROVIDER_NAME;
use super::commands::register_commands;
use super::parse::parse_interaction;
use super::render::{render_help, render_reply};

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
        let Interaction::Command(cmd) = interaction else {
            return;
        };
        let contents = if cmd.data.name == "help" {
            render_help()
        } else {
            match parse_interaction(&cmd) {
                Ok(command) => {
                    let user = ProviderUserRef {
                        provider: PROVIDER_NAME.to_string(),
                        user_id: cmd.user.id.get().to_string(),
                    };
                    match self.service.handle(&user, command).await {
                        Ok(reply) => render_reply(&reply),
                        Err(e) => {
                            error!(error = %format!("{e:#}"), "command handling failed");
                            vec![
                                "Error: something went wrong internally, please try again later."
                                    .to_string(),
                            ]
                        }
                    }
                }
                Err(e) => chunk_lines(&[format!("Error: {e:#}")], DISCORD_CHUNK_BUDGET),
            }
        };
        if let Err(e) = reply_ephemeral(&ctx, &cmd, &contents).await {
            error!(error = %format!("{e:#}"), "failed to send interaction response");
        }
    }
}

async fn reply_ephemeral(
    ctx: &Context,
    cmd: &CommandInteraction,
    contents: &[String],
) -> Result<()> {
    let first = contents
        .first()
        .context("cannot send an empty interaction response")?;
    let response = CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .content(first)
            .ephemeral(true),
    );
    cmd.create_response(&ctx.http, response)
        .await
        .context("sending interaction response")?;
    for content in &contents[1..] {
        cmd.create_followup(
            &ctx.http,
            CreateInteractionResponseFollowup::new()
                .content(content)
                .ephemeral(true),
        )
        .await
        .context("sending interaction response follow-up")?;
    }
    Ok(())
}
