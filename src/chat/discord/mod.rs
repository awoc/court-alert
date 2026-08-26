use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use serenity::Client;
use serenity::all::{GatewayIntents, GuildId};
use serenity::async_trait;

use crate::alerts::DailyPruner;
use crate::chat::{ChatProvider, ReadySignal};
use crate::config::DiscordSettings;
use crate::model::ProviderUserRef;
use crate::ports::AlertMessageRepository;
use crate::subscriptions::SubscriptionService;

mod commands;
mod dm;
mod error_webhook;
mod format;
mod handler;
mod http;
mod parse;
mod render;
mod strike;
mod text;
mod webhook;

pub use error_webhook::DiscordErrorLayer;
pub use webhook::DiscordNotifier;

use dm::DiscordSender;
use handler::Handler;

pub const PROVIDER_NAME: &str = "discord";

pub const MAX_CLUB_CHOICES: usize = 25;

pub struct DiscordProvider {
    token: String,
    guild_id: Option<GuildId>,
    admin_ids: HashSet<ProviderUserRef>,
    messages: Arc<dyn AlertMessageRepository>,
    pruner: Arc<DailyPruner>,
}

impl DiscordProvider {
    pub fn new(
        settings: DiscordSettings,
        messages: Arc<dyn AlertMessageRepository>,
        pruner: Arc<DailyPruner>,
    ) -> Self {
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
            messages,
            pruner,
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
            commands_registered: AtomicBool::new(false),
        };
        let mut client = Client::builder(&self.token, GatewayIntents::empty())
            .event_handler(handler)
            .await
            .context("building serenity client")?;

        service.register_sender(
            PROVIDER_NAME,
            Arc::new(DiscordSender::new(
                client.http.clone(),
                self.messages.clone(),
                self.pruner.clone(),
            )),
        );

        client.start().await.context("starting serenity client")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn admin_ids_become_provider_scoped_user_refs() {
        let store = Arc::new(crate::store::SqliteStore::open_in_memory().await.unwrap());
        let provider = DiscordProvider::new(
            DiscordSettings {
                token: "t".into(),
                guild_id: Some(42),
                admin_ids: HashSet::from(["123".to_string()]),
            },
            store.clone(),
            Arc::new(DailyPruner::new(store)),
        );
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
