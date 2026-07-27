use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::domain::{AlertLine, AvailabilityChange};
use crate::ports::{AlertMessageRepository, AvailabilityChangeSink};

use super::discord_http::{
    HTTP_TIMEOUT, MAX_RATE_LIMIT_RETRIES, parse_retry_after, redact_discord_webhook_tokens,
    retry_delay,
};
use super::format::{added_slots, chunk_slots, removed_slot_ids, render};

/// Discord's "Unknown Message" error. The only 404 that means *this message*
/// is gone — 10015 ("Unknown Webhook") shares the status but means the
/// credential is wrong, and every tracked row is still valid.
const DISCORD_UNKNOWN_MESSAGE: i64 = 10008;

/// What a PATCH told us about the message we tried to edit.
enum EditOutcome {
    Edited,
    Gone,
}

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

    /// The webhook URL plus `messages/{id}`. Built through `path_segments_mut`
    /// rather than string concatenation: the POST URL carries `?wait=true`, and
    /// appending a path to a URL with a query would corrupt it.
    fn edit_url(&self, message_id: &str) -> Result<reqwest::Url> {
        let mut url = self.webhook_url.clone();
        url.path_segments_mut()
            .map_err(|()| anyhow!("Discord webhook URL cannot be a base"))?
            .extend(["messages", message_id]);
        Ok(url)
    }

    async fn edit(&self, message_id: &str, content: &str) -> Result<EditOutcome> {
        #[derive(Serialize)]
        struct Body<'a> {
            content: &'a str,
        }

        #[derive(Deserialize)]
        struct DiscordError {
            code: Option<i64>,
        }

        let url = self.edit_url(message_id)?;
        for attempt in 0..=MAX_RATE_LIMIT_RETRIES {
            let response = self
                .client
                .patch(url.clone())
                .json(&Body { content })
                .send()
                .await
                .map_err(reqwest::Error::without_url)
                .context("editing a Discord webhook message")?;
            let status = response.status();
            let header_delay = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_retry_after);
            let body = response.text().await.unwrap_or_default();

            if status.is_success() {
                return Ok(EditOutcome::Edited);
            }
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt < MAX_RATE_LIMIT_RETRIES
            {
                tokio::time::sleep(retry_delay(header_delay, &body)).await;
                continue;
            }
            if status == reqwest::StatusCode::NOT_FOUND {
                let code = serde_json::from_str::<DiscordError>(&body)
                    .ok()
                    .and_then(|error| error.code);
                // Anything we cannot positively identify as "Unknown Message"
                // is treated as the conservative case and keeps its rows.
                if code == Some(DISCORD_UNKNOWN_MESSAGE) {
                    return Ok(EditOutcome::Gone);
                }
            }

            let body = redact_discord_webhook_tokens(&body);
            anyhow::bail!("Discord webhook returned {status}: {body}");
        }
        unreachable!("rate-limit retry loop always returns or fails")
    }

    async fn strike_removed(&self, changes: &[AvailabilityChange]) {
        let removed = removed_slot_ids(changes);
        if removed.is_empty() {
            return;
        }
        let plans = match self.messages.plan_strikes(&removed).await {
            Ok(plans) => plans,
            Err(error) => {
                warn!(
                    error = %format!("{error:#}"),
                    "discord: planning strikethroughs failed; no edits attempted"
                );
                return;
            }
        };
        if plans.is_empty() {
            debug!(
                slots = removed.len(),
                "discord: no tracked message holds these slots; nothing to strike"
            );
            return;
        }

        // Sequential on purpose: a Pi 2 gains nothing from concurrent edits, and
        // Discord rate-limits per webhook anyway.
        for plan in plans {
            let message_id = plan.message.id;
            match self.edit(&message_id, &render(&plan.message.lines)).await {
                Ok(EditOutcome::Edited) => {
                    if let Err(error) = self
                        .messages
                        .commit_strikes(&message_id, &plan.newly_struck)
                        .await
                    {
                        warn!(
                            error = %format!("{error:#}"),
                            message_id,
                            "discord: edit succeeded but recording it failed"
                        );
                    }
                }
                Ok(EditOutcome::Gone) => {
                    debug!(
                        message_id,
                        "discord: message no longer exists; forgetting it"
                    );
                    if let Err(error) = self.messages.forget_message(&message_id).await {
                        warn!(
                            error = %format!("{error:#}"),
                            message_id,
                            "discord: forgetting a deleted message failed"
                        );
                    }
                }
                Err(error) => warn!(
                    error = %format!("{error:#}"),
                    message_id,
                    "discord: striking through an alert failed; it stays live-looking"
                ),
            }
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
        self.strike_removed(changes).await;
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use uuid::Uuid;
    use wiremock::matchers::{body_string_contains, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

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
            .expect(1)
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

    fn unknown_message_body() -> serde_json::Value {
        serde_json::json!({ "message": "Unknown Message", "code": 10008 })
    }

    fn unknown_webhook_body() -> serde_json::Value {
        serde_json::json!({ "message": "Unknown Webhook", "code": 10015 })
    }

    /// Records one message holding `slot`, without going through the network.
    async fn seed(store: &Arc<SqliteStore>, message_id: &str, slot: &BookableSlot) {
        store
            .record_message(message_id, &[AlertLine::from(slot)])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_removed_slot_is_struck_in_the_message_that_announced_it() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/webhooks/123/token/messages/1408"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let (notifier, store) = notifier(&server).await;
        let gone = slot("Court 2", 18);
        seed(&store, "1408", &gone).await;

        notifier
            .publish(&[AvailabilityChange::BecameUnbookable(gone.clone())])
            .await
            .unwrap();

        assert!(
            store
                .plan_strikes(&[BookableSlotId::from(&gone)])
                .await
                .unwrap()
                .is_empty(),
            "the strike was committed after Discord confirmed the edit"
        );
    }

    /// The whole point of the feature: the edited message must actually carry
    /// the strikethrough. Every other test here only proves a PATCH happened.
    /// The mock matches on the body, so a request without `~~` does not match,
    /// falls through to wiremock's 404, and fails this mock's `expect(1)`.
    #[tokio::test]
    async fn the_edit_body_contains_the_struck_line() {
        let server = MockServer::start().await;
        let gone = slot("Court 2", 18);
        let struck = format!("~~{}~~", "• Court 2 : Tue, 02.06.2026 20:00–21:00");
        Mock::given(method("PATCH"))
            .and(path("/api/webhooks/123/token/messages/1408"))
            .and(body_string_contains(&struck))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let (notifier, store) = notifier(&server).await;
        seed(&store, "1408", &gone).await;

        notifier
            .publish(&[AvailabilityChange::BecameUnbookable(gone)])
            .await
            .unwrap();
    }

    /// A message keeps its live lines when only one of them is struck.
    #[tokio::test]
    async fn only_the_removed_line_is_struck() {
        let server = MockServer::start().await;
        let staying = slot("Court 1", 8);
        let gone = slot("Court 2", 18);
        Mock::given(method("PATCH"))
            .and(body_string_contains(
                "• Court 1 : Tue, 02.06.2026 10:00–11:00\\n~~",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let (notifier, store) = notifier(&server).await;
        store
            .record_message("1408", &[AlertLine::from(&staying), AlertLine::from(&gone)])
            .await
            .unwrap();

        notifier
            .publish(&[AvailabilityChange::BecameUnbookable(gone)])
            .await
            .unwrap();
    }

    #[derive(Clone, Default)]
    struct RateLimitOnce(Arc<AtomicUsize>);

    impl Respond for RateLimitOnce {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(429).set_body_json(serde_json::json!({ "retry_after": 0.0 }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({}))
            }
        }
    }

    #[tokio::test]
    async fn a_rate_limited_edit_is_retried_and_then_committed() {
        let server = MockServer::start().await;
        let responder = RateLimitOnce::default();
        Mock::given(method("PATCH"))
            .respond_with(responder.clone())
            .expect(2)
            .mount(&server)
            .await;
        let (notifier, store) = notifier(&server).await;
        let gone = slot("Court 2", 18);
        seed(&store, "1408", &gone).await;

        notifier
            .publish(&[AvailabilityChange::BecameUnbookable(gone.clone())])
            .await
            .unwrap();

        assert_eq!(
            responder.0.load(Ordering::SeqCst),
            2,
            "one 429, then a retry"
        );
        assert!(
            store
                .plan_strikes(&[BookableSlotId::from(&gone)])
                .await
                .unwrap()
                .is_empty(),
            "the retried edit was committed"
        );
    }

    #[tokio::test]
    async fn a_failed_edit_leaves_the_row_unstruck() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let (notifier, store) = notifier(&server).await;
        let gone = slot("Court 2", 18);
        seed(&store, "1408", &gone).await;

        notifier
            .publish(&[AvailabilityChange::BecameUnbookable(gone.clone())])
            .await
            .unwrap();

        assert_eq!(
            store
                .plan_strikes(&[BookableSlotId::from(&gone)])
                .await
                .unwrap()
                .len(),
            1,
            "nothing is committed when Discord did not confirm"
        );
    }

    #[tokio::test]
    async fn a_deleted_message_is_forgotten() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(404).set_body_json(unknown_message_body()))
            .mount(&server)
            .await;
        let (notifier, store) = notifier(&server).await;
        let gone = slot("Court 2", 18);
        seed(&store, "1408", &gone).await;

        notifier
            .publish(&[AvailabilityChange::BecameUnbookable(gone.clone())])
            .await
            .unwrap();

        assert!(
            store
                .plan_strikes(&[BookableSlotId::from(&gone)])
                .await
                .unwrap()
                .is_empty(),
            "rows for a message Discord no longer knows are dropped"
        );
    }

    /// A rotated webhook 404s every edit. Deleting rows on that would turn a
    /// config typo into permanent data loss, so only code 10008 forgets.
    #[tokio::test]
    async fn an_unknown_webhook_does_not_delete_anything() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(404).set_body_json(unknown_webhook_body()))
            .mount(&server)
            .await;
        let (notifier, store) = notifier(&server).await;
        let gone = slot("Court 2", 18);
        seed(&store, "1408", &gone).await;

        notifier
            .publish(&[AvailabilityChange::BecameUnbookable(gone.clone())])
            .await
            .unwrap();

        assert_eq!(
            store
                .plan_strikes(&[BookableSlotId::from(&gone)])
                .await
                .unwrap()
                .len(),
            1,
            "the rows survive a wrong-credential 404"
        );
    }

    #[tokio::test]
    async fn an_unparseable_404_keeps_the_rows() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(404).set_body_string("<html>nope</html>"))
            .mount(&server)
            .await;
        let (notifier, store) = notifier(&server).await;
        let gone = slot("Court 2", 18);
        seed(&store, "1408", &gone).await;

        notifier
            .publish(&[AvailabilityChange::BecameUnbookable(gone.clone())])
            .await
            .unwrap();

        assert_eq!(
            store
                .plan_strikes(&[BookableSlotId::from(&gone)])
                .await
                .unwrap()
                .len(),
            1
        );
    }
}
