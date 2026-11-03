use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use futures::StreamExt;
use serenity::all::{
    GatewayIntents, GuildId, Http, PresenceData, ShardManager, ShardManagerOptions,
};
use serenity::async_trait;
use serenity::prelude::{Mutex, RwLock, TypeMap};
use tracing::warn;

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
const DEFAULT_GATEWAY_URL: &str = "wss://gateway.discord.gg";

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
    http: Arc<Http>,
    guild_id: Option<GuildId>,
    admin_ids: HashSet<SubscriberRef>,
    alert_lifecycle: Arc<AlertMessageLifecycle>,
}

impl DiscordIntegration {
    pub fn new(settings: DiscordSettings, alert_lifecycle: Arc<AlertMessageLifecycle>) -> Self {
        Self {
            http: Arc::new(Http::new(&settings.token)),
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
        services.dispatcher.register_chat_provider(
            INTEGRATION,
            Arc::new(DiscordSender::new(
                self.http.clone(),
                self.alert_lifecycle.clone(),
            )),
        );

        let ws_url = match self.http.get_gateway().await {
            Ok(gateway) => gateway.url,
            Err(error) => {
                warn!(error = %error, "fetching the Discord gateway URL failed; using the default");
                DEFAULT_GATEWAY_URL.to_owned()
            }
        };
        let (shards, mut outcome) = ShardManager::new(ShardManagerOptions {
            data: Arc::new(RwLock::new(TypeMap::new())),
            event_handlers: vec![Arc::new(handler)],
            raw_event_handlers: vec![],
            shard_index: 0,
            shard_init: 1,
            shard_total: 1,
            ws_url: Arc::new(Mutex::new(ws_url)),
            http: self.http,
            intents: GatewayIntents::empty(),
            presence: Some(PresenceData::default()),
        });
        shards.initialize().context("starting serenity client")?;
        if let Some(Err(error)) = outcome.next().await {
            return Err(serenity::Error::Gateway(error)).context("starting serenity client");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::integrations::readiness;
    use crate::store::SqliteStore;
    use crate::subscriptions::testing::registry;
    use crate::subscriptions::{AlertDispatcher, BerlinClock, ClubDirectory, SubscriptionService};
    use futures::SinkExt;
    use serenity::all::{ApplicationId, HttpBuilder};
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
    const APPLICATION_ID: u64 = 4242;
    const AUTHENTICATION_FAILED: u16 = 4004;

    enum GatewayAnswer {
        Ready,
        Close(u16),
    }

    /// Greets one connection, waits for it to identify, then answers.
    async fn fake_gateway(answer: GatewayAnswer) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let send = |payload: serde_json::Value| Message::Text(payload.to_string());
            socket
                .send(send(
                    serde_json::json!({"op": 10, "d": {"heartbeat_interval": 45000}}),
                ))
                .await
                .unwrap();
            loop {
                let message = socket.next().await.unwrap().unwrap();
                let identified = message
                    .to_text()
                    .ok()
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                    .is_some_and(|payload| payload["op"] == 2);
                if identified {
                    break;
                }
            }
            match answer {
                GatewayAnswer::Ready => {
                    socket
                        .send(send(serde_json::json!({
                            "op": 0,
                            "s": 1,
                            "t": "READY",
                            "d": {
                                "v": 10,
                                "user": {
                                    "id": "1",
                                    "username": "court-alert",
                                    "discriminator": "0001",
                                    "avatar": null,
                                    "bot": true,
                                },
                                "guilds": [],
                                "session_id": "session",
                                "resume_gateway_url": "ws://127.0.0.1:9",
                                "shard": [0, 1],
                                "application": {"id": APPLICATION_ID.to_string(), "flags": 0},
                            },
                        })))
                        .await
                        .unwrap();
                    std::future::pending::<()>().await;
                }
                GatewayAnswer::Close(code) => {
                    socket
                        .send(Message::Close(Some(CloseFrame {
                            code: CloseCode::from(code),
                            reason: "closed by the test".into(),
                        })))
                        .await
                        .unwrap();
                    while socket.next().await.is_some() {}
                }
            }
        });
        url
    }

    async fn discord_api_pointing_at(gateway_url: &str) -> MockServer {
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v10/gateway"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"url": gateway_url})),
            )
            .mount(&api)
            .await;
        api
    }

    async fn integration_against(api: &MockServer) -> DiscordIntegration {
        let store = Arc::new(SqliteStore::open_in_memory().await.unwrap());
        DiscordIntegration {
            http: Arc::new(
                HttpBuilder::new("token")
                    .proxy(api.uri())
                    .ratelimiter_disabled(true)
                    .build(),
            ),
            guild_id: None,
            admin_ids: HashSet::new(),
            alert_lifecycle: Arc::new(AlertMessageLifecycle::new(store)),
        }
    }

    async fn services() -> IntegrationServices {
        let store = Arc::new(SqliteStore::open_in_memory().await.unwrap());
        let registry = registry();
        IntegrationServices {
            subscriptions: Arc::new(SubscriptionService::new(
                store.clone(),
                store.clone(),
                HashSet::new(),
                registry.clone(),
                crate::model::CourtFilter::Any,
                Arc::new(BerlinClock),
            )),
            clubs: ClubDirectory::new(registry.clone()),
            dispatcher: Arc::new(AlertDispatcher::new(store, registry, Arc::new(BerlinClock))),
        }
    }

    #[tokio::test]
    async fn the_gateway_shares_the_integrations_http_client_and_signals_readiness() {
        let gateway = fake_gateway(GatewayAnswer::Ready).await;
        let api = discord_api_pointing_at(&gateway).await;
        let integration = integration_against(&api).await;
        let http = integration.http.clone();
        assert_eq!(http.application_id(), None);
        let (mut signals, barrier) = readiness(1);

        tokio::spawn(Box::new(integration).run(services().await, signals.remove(0)));

        tokio::time::timeout(Duration::from_secs(10), barrier.wait())
            .await
            .expect("the gateway never reported ready");
        assert_eq!(
            http.application_id(),
            Some(ApplicationId::new(APPLICATION_ID)),
            "serenity learns the application ID from the gateway and stores it on the Http it \
             was given, so this one was not it"
        );
    }

    #[tokio::test]
    async fn a_fatal_gateway_error_ends_the_integration() {
        let gateway = fake_gateway(GatewayAnswer::Close(AUTHENTICATION_FAILED)).await;
        let api = discord_api_pointing_at(&gateway).await;
        let integration = integration_against(&api).await;
        let (mut signals, _barrier) = readiness(1);

        let error = tokio::time::timeout(
            Duration::from_secs(10),
            Box::new(integration).run(services().await, signals.remove(0)),
        )
        .await
        .expect("a rejected token must not leave the integration running")
        .expect_err("a rejected token is fatal");

        assert!(
            format!("{error:#}").starts_with("starting serenity client"),
            "unexpected error: {error:#}"
        );
    }
}
