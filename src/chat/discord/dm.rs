use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use serenity::all::{ChannelId, EditMessage, Http, HttpError, MessageId, StatusCode, UserId};
use serenity::async_trait;

use crate::alerts::{AlertLifecycle, AlertTracker, EditOutcome};
use crate::model::{AlertSurface, BookableSlotId};
use crate::subscriptions::contract::{AvailabilityAlert, DirectMessageSender};

use super::format::{alert_lines, chunk_lines, render};
use super::{DISCORD_UNKNOWN_MESSAGE, PROVIDER_NAME};

pub(super) struct DiscordSender {
    http: Arc<Http>,
    alerts: AlertTracker,
}

impl DiscordSender {
    pub(super) fn new(http: Arc<Http>, alerts: Arc<AlertLifecycle>) -> Self {
        Self {
            http,
            alerts: alerts.tracker(PROVIDER_NAME, AlertSurface::DirectMessage),
        }
    }

    async fn edit(&self, channel_id: &str, message_id: &str, content: &str) -> Result<EditOutcome> {
        let channel = ChannelId::from_str(channel_id)
            .with_context(|| format!("invalid Discord channel id {channel_id:?}"))?;
        let message = MessageId::from_str(message_id)
            .with_context(|| format!("invalid Discord message id {message_id:?}"))?;
        match channel
            .edit_message(&self.http, message, EditMessage::new().content(content))
            .await
        {
            Ok(_) => Ok(EditOutcome::Edited),
            Err(serenity::Error::Http(HttpError::UnsuccessfulRequest(response)))
                if response.status_code == StatusCode::NOT_FOUND
                    && response.error.code as i64 == DISCORD_UNKNOWN_MESSAGE =>
            {
                Ok(EditOutcome::Gone)
            }
            Err(error) => Err(error).context("editing a direct message"),
        }
    }
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
        for chunk in chunk_lines(alert_lines(alert)) {
            let sent = channel
                .say(&self.http, render(&chunk))
                .await
                .context("sending DM")?;
            self.alerts
                .record(Some(&channel.id.to_string()), &sent.id.to_string(), &chunk)
                .await;
        }
        Ok(())
    }

    async fn strike_taken(&self, slots: &[BookableSlotId]) -> Result<()> {
        self.alerts
            .strike_taken(slots, |message| async move {
                let channel = message.key.destination.as_deref().context(
                    "a recorded direct message has no channel, so it cannot be edited again",
                )?;
                self.edit(channel, &message.key.id, &render(&message.lines))
                    .await
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AlertLine, AlertMessageKey, BookableSlot, VenueId};
    use crate::ports::AlertMessageRepository;
    use crate::store::SqliteStore;
    use crate::subscriptions::contract::AvailableSlotSummary;
    use chrono::{TimeZone, Utc};
    use serenity::all::HttpBuilder;
    use uuid::Uuid;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const DM_CHANNEL: &str = "77";

    fn user_json() -> serde_json::Value {
        serde_json::json!({
            "id": "1",
            "username": "court-alert",
            "discriminator": "0001",
            "avatar": null,
            "bot": true,
        })
    }

    fn message_json(id: &str, content: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "channel_id": DM_CHANNEL,
            "author": user_json(),
            "content": content,
            "timestamp": "2026-06-01T12:00:00.000000+00:00",
            "edited_timestamp": null,
            "tts": false,
            "mention_everyone": false,
            "mentions": [],
            "mention_roles": [],
            "attachments": [],
            "embeds": [],
            "pinned": false,
            "webhook_id": null,
            "type": 0,
            "activity": null,
            "application": null,
            "application_id": null,
            "message_reference": null,
            "flags": null,
            "referenced_message": null,
            "interaction": null,
            "interaction_metadata": null,
            "thread": null,
            "position": null,
            "role_subscription_data": null,
        })
    }

    async fn sender(server: &MockServer) -> (DiscordSender, Arc<SqliteStore>) {
        let store = Arc::new(SqliteStore::open_in_memory().await.unwrap());
        let http = HttpBuilder::new("Bot token")
            .proxy(server.uri())
            .ratelimiter_disabled(true)
            .build();
        let alerts = Arc::new(AlertLifecycle::new(store.clone()));
        (DiscordSender::new(Arc::new(http), alerts), store)
    }

    fn slot(court: &str) -> BookableSlot {
        let starts_at = Utc.with_ymd_and_hms(2026, 6, 2, 18, 0, 0).unwrap();
        BookableSlot {
            venue_id: VenueId::new("zhs-munich"),
            court_id: Uuid::new_v4(),
            court_name: court.into(),
            starts_at,
            ends_at: starts_at + chrono::Duration::hours(1),
            available_places: 1,
        }
    }

    fn alert(slots: &[&BookableSlot]) -> AvailabilityAlert {
        AvailabilityAlert {
            slots: slots
                .iter()
                .map(|slot| AvailableSlotSummary {
                    club: "ZHS München".into(),
                    court: slot.court_name.clone(),
                    court_id: slot.court_id,
                    starts_at: slot.starts_at,
                    ends_at: slot.ends_at,
                })
                .collect(),
        }
    }

    async fn mock_dm_channel(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/api/v10/users/@me/channels"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": DM_CHANNEL,
                "type": 1,
                "recipients": [user_json()],
            })))
            .mount(server)
            .await;
    }

    async fn seed(store: &Arc<SqliteStore>, message_id: &str, slot: &BookableSlot) {
        store
            .record_message(
                &AlertMessageKey::new(
                    "discord",
                    AlertSurface::DirectMessage,
                    Some(DM_CHANNEL),
                    message_id,
                ),
                &[AlertLine {
                    club: Some("ZHS München".into()),
                    ..AlertLine::from(slot)
                }],
            )
            .await
            .unwrap();
    }

    async fn plans(store: &Arc<SqliteStore>, slot: &BookableSlot) -> Vec<crate::model::StrikePlan> {
        store
            .plan_strikes(
                "discord",
                AlertSurface::DirectMessage,
                &[crate::model::BookableSlotId::from(slot)],
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_sent_dm_is_recorded_with_the_channel_it_can_be_edited_in() {
        let server = MockServer::start().await;
        mock_dm_channel(&server).await;
        Mock::given(method("POST"))
            .and(path(format!("/api/v10/channels/{DM_CHANNEL}/messages")))
            .and(body_string_contains("ZHS München"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(message_json("1408", "an alert")),
            )
            .expect(1)
            .mount(&server)
            .await;
        let (sender, store) = sender(&server).await;
        let announced = slot("Court 2");

        sender
            .send_dm("12345", &alert(&[&announced]))
            .await
            .unwrap();

        let plans = plans(&store, &announced).await;
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].message.key.id, "1408");
        assert_eq!(
            plans[0].message.key.destination.as_deref(),
            Some(DM_CHANNEL)
        );
        assert_eq!(
            plans[0].message.lines[0].club.as_deref(),
            Some("ZHS München"),
            "the club has to survive, or the edit would drop it from the line"
        );
    }

    #[tokio::test]
    async fn a_booked_court_is_struck_through_in_the_dm_that_announced_it() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path(format!(
                "/api/v10/channels/{DM_CHANNEL}/messages/1408"
            )))
            .and(body_string_contains(
                "~~ZHS München — Court 02: Tue, 02.06.2026 20:00–21:00~~",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(message_json("1408", "struck")))
            .expect(1)
            .mount(&server)
            .await;
        let (sender, store) = sender(&server).await;
        sender.alerts.skip_pruning_today();
        let taken = slot("Court 2");
        seed(&store, "1408", &taken).await;

        sender
            .strike_taken(&[crate::model::BookableSlotId::from(&taken)])
            .await
            .unwrap();

        assert!(
            plans(&store, &taken).await.is_empty(),
            "the strike was committed after Discord confirmed the edit"
        );
    }

    #[tokio::test]
    async fn a_failed_edit_leaves_the_line_to_be_struck_again() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let (sender, store) = sender(&server).await;
        sender.alerts.skip_pruning_today();
        let taken = slot("Court 2");
        seed(&store, "1408", &taken).await;

        sender
            .strike_taken(&[crate::model::BookableSlotId::from(&taken)])
            .await
            .unwrap();

        assert_eq!(plans(&store, &taken).await.len(), 1);
    }

    #[tokio::test]
    async fn a_dm_the_user_deleted_is_forgotten() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "message": "Unknown Message",
                "code": 10008,
            })))
            .mount(&server)
            .await;
        let (sender, store) = sender(&server).await;
        sender.alerts.skip_pruning_today();
        let taken = slot("Court 2");
        seed(&store, "1408", &taken).await;

        sender
            .strike_taken(&[crate::model::BookableSlotId::from(&taken)])
            .await
            .unwrap();

        assert!(
            plans(&store, &taken).await.is_empty(),
            "rows for a message Discord no longer knows are dropped"
        );
    }

    #[tokio::test]
    async fn an_unknown_message_code_without_a_404_keeps_the_rows() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "message": "Unknown Message",
                "code": 10008,
            })))
            .mount(&server)
            .await;
        let (sender, store) = sender(&server).await;
        sender.alerts.skip_pruning_today();
        let taken = slot("Court 2");
        seed(&store, "1408", &taken).await;

        sender
            .strike_taken(&[crate::model::BookableSlotId::from(&taken)])
            .await
            .unwrap();

        assert_eq!(plans(&store, &taken).await.len(), 1);
    }

    #[tokio::test]
    async fn striking_also_prunes_messages_whose_slots_have_ended() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let (sender, store) = sender(&server).await;
        let ended = slot("Court 2");
        seed(&store, "1408", &ended).await;

        sender
            .strike_taken(&[crate::model::BookableSlotId::from(&ended)])
            .await
            .unwrap();

        assert_eq!(
            sender.alerts.last_pruned(),
            Some(crate::time::today_berlin())
        );
        assert_eq!(
            store.prune_ended(Utc::now()).await.unwrap(),
            0,
            "striking already pruned the ended message"
        );
    }
}
