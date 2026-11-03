use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use serenity::Client;
use serenity::all::{GatewayIntents, GuildId};
use serenity::async_trait;

use crate::alerts::AlertMessageLifecycle;
use crate::config::{Config, DiscordSettings};
use crate::integrations::{Integration, IntegrationServices, ReadySignal};
use crate::model::{IntegrationId, Sport, SubscriberRef};

mod commands;
mod dm;
mod error_webhook;
mod format;
mod handler;
mod http;
mod parse;
mod render;
mod text;
mod webhook;

pub use error_webhook::DiscordErrorLayer;
pub use webhook::DiscordNotifier;

use dm::DiscordSender;
use handler::Handler;

const DISCORD_UNKNOWN_MESSAGE: i64 = 10008;

pub const INTEGRATION: IntegrationId = IntegrationId::from_static("discord");

pub const MAX_CLUB_CHOICES: usize = 25;

pub fn validate_configuration(config: &Config) -> Result<()> {
    let padel_venues = config
        .venues()
        .iter()
        .filter(|venue| venue.sport == Sport::Padel)
        .count();
    anyhow::ensure!(
        padel_venues <= MAX_CLUB_CHOICES,
        "{padel_venues} padel venues are configured, but Discord allows at most \
         {MAX_CLUB_CHOICES} club choices on /padel; monitoring more would leave the rest \
         selectable only through \"all clubs\""
    );
    Ok(())
}

pub struct DiscordIntegration {
    token: String,
    guild_id: Option<GuildId>,
    admin_ids: HashSet<SubscriberRef>,
    alert_lifecycle: Arc<AlertMessageLifecycle>,
}

impl DiscordIntegration {
    pub fn new(settings: DiscordSettings, alert_lifecycle: Arc<AlertMessageLifecycle>) -> Self {
        Self {
            token: settings.token,
            guild_id: settings.guild_id.map(GuildId::new),
            admin_ids: settings
                .admin_ids
                .into_iter()
                .map(|id| SubscriberRef {
                    integration: INTEGRATION,
                    subscriber_id: id,
                })
                .collect(),
            alert_lifecycle,
        }
    }
}

#[async_trait]
impl Integration for DiscordIntegration {
    fn admins(&self) -> HashSet<SubscriberRef> {
        self.admin_ids.clone()
    }

    async fn run(self: Box<Self>, services: IntegrationServices, ready: ReadySignal) -> Result<()> {
        let handler = Handler {
            service: services.subscriptions,
            clubs: services.clubs,
            guild_id: self.guild_id,
            ready,
            commands_registered: AtomicBool::new(false),
        };
        let mut client = Client::builder(&self.token, GatewayIntents::empty())
            .event_handler(handler)
            .await
            .context("building serenity client")?;

        services.dispatcher.register_chat_provider(
            INTEGRATION,
            Arc::new(DiscordSender::new(
                client.http.clone(),
                self.alert_lifecycle.clone(),
            )),
        );

        client.start().await.context("starting serenity client")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZHS: &str = r#"
poll_interval_secs = 300
lookahead_days = 7

[[venues]]
id = "zhs-munich"
display_name = "ZHS München"
sport = "tennis"
provider = "zhs"
base_url = "https://kurse.zhs-muenchen.de"

  [[venues.courts]]
  id = "92db7384-2dec-4888-a92a-4c2b6faac5f7"
  name = "Tennis Court 1"
"#;

    fn config_with_padel_clubs(count: usize) -> Config {
        let clubs: String = (0..count)
            .map(|i| {
                format!(
                    "\n[[venues]]\nid = \"club-{i}\"\ndisplay_name = \"Club {i}\"\n\
                     sport = \"padel\"\nprovider = \"playtomic\"\n\
                     tenant_id = \"f8483f72-1d14-49eb-a98b-e4b89d969c78\"\n\
                     slug = \"club-{i}\"\nlookahead_days = 15\n"
                )
            })
            .collect();
        Config::parse(&format!("{ZHS}{clubs}")).expect("parse")
    }

    #[test]
    fn more_padel_venues_than_discord_can_offer_are_rejected() {
        let with = |count| validate_configuration(&config_with_padel_clubs(count));

        assert!(
            with(MAX_CLUB_CHOICES).is_ok(),
            "{MAX_CLUB_CHOICES} clubs must still be accepted"
        );

        let error = with(MAX_CLUB_CHOICES + 1).expect_err("more clubs than Discord can offer");
        assert!(
            format!("{error:#}").contains(&MAX_CLUB_CHOICES.to_string()),
            "the limit is not named: {error:#}"
        );
    }

    #[tokio::test]
    async fn admin_ids_become_integration_scoped_subscriber_refs() {
        let store = Arc::new(crate::store::SqliteStore::open_in_memory().await.unwrap());
        let integration = DiscordIntegration::new(
            DiscordSettings {
                token: "t".into(),
                guild_id: Some(42),
                admin_ids: HashSet::from(["123".to_string()]),
            },
            Arc::new(AlertMessageLifecycle::new(store)),
        );
        assert_eq!(
            integration.admins(),
            HashSet::from([SubscriberRef {
                integration: INTEGRATION,
                subscriber_id: "123".to_string(),
            }])
        );
        assert_eq!(integration.guild_id, Some(GuildId::new(42)));
    }
}
