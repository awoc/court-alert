use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::domain::{AlertLine, AvailabilityChange};
use crate::ports::{AlertMessageRepository, AvailabilityChangeSink};

use super::discord_http::{HTTP_TIMEOUT, redact_discord_webhook_tokens};
use super::format::{added_slots, chunk_slots, render};

pub struct DiscordNotifier {
    webhook_url: reqwest::Url,
    client: reqwest::Client,
    messages: Arc<dyn AlertMessageRepository>,
    /// Berlin date of the last prune. Pruning is a housekeeping delete; running
    /// it once a day rather than once a tick keeps SD-card writes down.
    last_pruned: Mutex<Option<NaiveDate>>,
}

impl DiscordNotifier {
    pub fn new(
        webhook_url: reqwest::Url,
        messages: Arc<dyn AlertMessageRepository>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .context("building Discord HTTP client")?;
        Ok(Self {
            webhook_url,
            client,
            messages,
            last_pruned: Mutex::new(None),
        })
    }

    /// Posts a message and returns the id Discord assigned it. `?wait=true` is
    /// what makes Discord answer with the created message instead of `204`.
    async fn post(&self, content: &str) -> Result<String> {
        #[derive(Serialize)]
        struct Body<'a> {
            content: &'a str,
        }

        #[derive(Deserialize)]
        struct Created {
            id: String,
        }

        let mut url = self.webhook_url.clone();
        url.query_pairs_mut().append_pair("wait", "true");

        let response = self
            .client
            .post(url)
            .json(&Body { content })
            .send()
            .await
            .map_err(reqwest::Error::without_url)
            .context("posting to Discord webhook")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let body = redact_discord_webhook_tokens(&body);
            anyhow::bail!("Discord webhook returned {status}: {body}");
        }
        serde_json::from_str::<Created>(&body)
            .map(|created| created.id)
            .context("reading the message id from the Discord webhook response")
    }

    async fn post_added(&self, changes: &[AvailabilityChange]) {
        for chunk in chunk_slots(&added_slots(changes)) {
            let content = render(&chunk);
            debug!(
                lines = chunk.len(),
                bytes = content.len(),
                "discord: posting"
            );
            match self.post(&content).await {
                Ok(message_id) => self.record(&message_id, &chunk).await,
                Err(error) => warn!(
                    error = %format!("{error:#}"),
                    "discord: posting an alert failed; its slots cannot be struck later"
                ),
            }
        }
    }

    async fn record(&self, message_id: &str, lines: &[AlertLine]) {
        if let Err(error) = self.messages.record_message(message_id, lines).await {
            warn!(
                error = %format!("{error:#}"),
                message_id,
                "discord: recording an alert message failed; it cannot be edited later"
            );
        }
    }
}

#[async_trait]
impl AvailabilityChangeSink for DiscordNotifier {
    /// Best-effort by design: every failure is logged and skipped rather than
    /// propagated, so one bad message never aborts a poll tick.
    async fn publish(&self, changes: &[AvailabilityChange]) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        self.post_added(changes).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BookableSlot, BookableSlotId};
    use crate::store::SqliteStore;
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn slot(name: &str, hour: u32) -> BookableSlot {
        let starts_at = Utc.with_ymd_and_hms(2026, 6, 2, hour, 0, 0).unwrap();
        BookableSlot {
            court_id: Uuid::new_v4(),
            court_name: name.into(),
            starts_at,
            ends_at: starts_at + chrono::Duration::hours(1),
            available_places: 1,
        }
    }

    async fn notifier(server: &MockServer) -> (DiscordNotifier, Arc<SqliteStore>) {
        let store = Arc::new(SqliteStore::open_in_memory().await.unwrap());
        let url = format!("{}/api/webhooks/123/token", server.uri())
            .parse()
            .unwrap();
        (DiscordNotifier::new(url, store.clone()).unwrap(), store)
    }

    #[tokio::test]
    async fn posting_waits_for_the_message_id_and_records_the_lines() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/webhooks/123/token"))
            .and(query_param("wait", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "1408"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (notifier, store) = notifier(&server).await;
        let added = slot("Court 2", 18);

        notifier
            .publish(&[AvailabilityChange::BecameBookable(added.clone())])
            .await
            .unwrap();

        let plans = store
            .plan_strikes(&[BookableSlotId::from(&added)])
            .await
            .unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].message.id, "1408", "the id came from ?wait=true");
    }

    #[tokio::test]
    async fn a_failed_post_records_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let (notifier, store) = notifier(&server).await;
        let added = slot("Court 2", 18);

        notifier
            .publish(&[AvailabilityChange::BecameBookable(added.clone())])
            .await
            .unwrap();

        assert!(
            store
                .plan_strikes(&[BookableSlotId::from(&added)])
                .await
                .unwrap()
                .is_empty()
        );
    }
}
