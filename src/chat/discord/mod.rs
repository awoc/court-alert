//! Discord: the slash-command bot, the direct messages it sends, and the
//! broadcast webhook. One service, one module — the bot and the webhook share
//! its rendering and its HTTP quirks.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context, Result};
use serenity::Client;
use serenity::all::{GatewayIntents, GuildId, Http, UserId};
use serenity::async_trait;

use crate::chat::{ChatProvider, ReadySignal};
use crate::config::DiscordSettings;
use crate::model::ProviderUserRef;
use crate::subscriptions::SubscriptionService;
use crate::subscriptions::contract::{AvailabilityAlert, DirectMessageSender};

mod commands;
mod error_webhook;
mod format;
mod handler;
mod http;
mod parse;
mod render;
mod webhook;

pub use error_webhook::DiscordErrorLayer;
pub use webhook::DiscordNotifier;

use handler::Handler;
use render::render_alert;

pub const PROVIDER_NAME: &str = "discord";

/// Discord caps a string command option at 25 choices, which bounds how many
/// clubs `/padel` can offer by name. Config refuses to start beyond it rather
/// than registering a command that silently omits the rest.
pub const MAX_CLUB_CHOICES: usize = 25;

pub struct DiscordProvider {
    token: String,
    guild_id: Option<GuildId>,
    admin_ids: HashSet<ProviderUserRef>,
}

impl DiscordProvider {
    pub fn new(settings: DiscordSettings) -> Self {
        Self {
            token: settings.token,
            guild_id: settings.guild_id.map(GuildId::new),
            admin_ids: settings
                .admin_ids
                .into_iter()
                .map(|id| ProviderUserRef {
                    provider: PROVIDER_NAME.to_string(),
                    user_id: id,
                })
                .collect(),
        }
    }
}

#[async_trait]
impl ChatProvider for DiscordProvider {
    fn admins(&self) -> HashSet<ProviderUserRef> {
        self.admin_ids.clone()
    }

    async fn run(
        self: Box<Self>,
        service: Arc<SubscriptionService>,
        ready: ReadySignal,
    ) -> Result<()> {
        let handler = Handler {
            service: service.clone(),
            guild_id: self.guild_id,
            ready,
        };
        let mut client = Client::builder(&self.token, GatewayIntents::empty())
            .event_handler(handler)
            .await
            .context("building serenity client")?;

        service.register_sender(
            PROVIDER_NAME,
            Arc::new(DiscordSender {
                http: client.http.clone(),
            }),
        );

        client.start().await.context("starting serenity client")?;
        Ok(())
    }
}

struct DiscordSender {
    http: Arc<Http>,
}

#[async_trait]
impl DirectMessageSender for DiscordSender {
    async fn send_dm(&self, user_id: &str, alert: &AvailabilityAlert) -> Result<()> {
        let id: u64 = user_id
            .parse()
            .with_context(|| format!("invalid Discord user id {user_id:?}"))?;
        let channel = UserId::new(id)
            .create_dm_channel(&self.http)
            .await
            .context("opening DM channel")?;
        for msg in render_alert(alert) {
            channel.say(&self.http, msg).await.context("sending DM")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_ids_become_provider_scoped_user_refs() {
        let provider = DiscordProvider::new(DiscordSettings {
            token: "t".into(),
            guild_id: Some(42),
            admin_ids: HashSet::from(["123".to_string()]),
        });
        assert_eq!(
            provider.admins(),
            HashSet::from([ProviderUserRef {
                provider: PROVIDER_NAME.to_string(),
                user_id: "123".to_string(),
            }])
        );
        assert_eq!(provider.guild_id, Some(GuildId::new(42)));
    }
}
