use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use chrono::{NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::model::{AlertLine, AvailabilityChange};
use crate::ports::{AlertMessageRepository, AvailabilityChangeSink};
use crate::time::today_berlin;

use super::format::{added_slots, chunk_slots, removed_slot_ids, render};
use super::http::{HTTP_TIMEOUT, redact_discord_webhook_tokens, send_with_rate_limit_retry};

// Other Discord 404 codes do not prove that the tracked message is gone.
const DISCORD_UNKNOWN_MESSAGE: i64 = 10008;

enum EditOutcome {
    Edited,
    Gone,
}

pub struct DiscordNotifier {
    webhook_url: reqwest::Url,
    client: reqwest::Client,
    messages: Arc<dyn AlertMessageRepository>,
    // Avoid repeated cleanup writes on SD-card storage.
    last_pruned: Mutex<Option<NaiveDate>>,
}

impl DiscordNotifier {
    pub fn new(
        mut webhook_url: reqwest::Url,
        messages: Arc<dyn AlertMessageRepository>,
    ) -> Result<Self> {
        // Avoid producing `.../token//messages/{id}` in `edit_url`.
        webhook_url
            .path_segments_mut()
            .map_err(|()| anyhow!("Discord webhook URL cannot be a base"))?
            .pop_if_empty();
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
        // Discord only returns the created message ID when `wait=true`.
        url.query_pairs_mut().append_pair("wait", "true");

        let (status, body) = send_with_rate_limit_retry(
            || self.client.post(url.clone()).json(&Body { content }),
            "posting to Discord webhook",
        )
        .await?;
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
        let (status, body) = send_with_rate_limit_retry(
            || self.client.patch(url.clone()).json(&Body { content }),
            "editing a Discord webhook message",
        )
        .await?;

        if status.is_success() {
            return Ok(EditOutcome::Edited);
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            let code = serde_json::from_str::<DiscordError>(&body)
                .ok()
                .and_then(|error| error.code);
            if code == Some(DISCORD_UNKNOWN_MESSAGE) {
                return Ok(EditOutcome::Gone);
            }
        }

        let body = redact_discord_webhook_tokens(&body);
        anyhow::bail!("Discord webhook returned {status}: {body}")
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

    async fn prune_daily(&self) {
        let today = today_berlin();
        if *self.last_pruned.lock().expect("prune guard poisoned") == Some(today) {
            return;
        }
        match self.messages.prune_ended(Utc::now()).await {
            Ok(removed) => {
                *self.last_pruned.lock().expect("prune guard poisoned") = Some(today);
                if removed > 0 {
                    debug!(
                        removed,
                        "discord: pruned alert messages whose slots all ended"
                    );
                }
            }
            Err(error) => warn!(
                error = %format!("{error:#}"),
                "discord: pruning alert messages failed"
            ),
        }
    }
}

#[async_trait]
impl AvailabilityChangeSink for DiscordNotifier {
    // Notification failures are logged per message rather than failing the poll tick.
    async fn publish(&self, changes: &[AvailabilityChange]) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        // Strike before pruning can drop old rows; prune before recording new rows.
        self.strike_removed(changes).await;
        self.prune_daily().await;
        self.post_added(changes).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BookableSlot, BookableSlotId};
    use crate::store::SqliteStore;
    use chrono::{TimeZone, Utc};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use uuid::Uuid;
    use wiremock::matchers::{body_string_contains, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn slot(name: &str, hour: u32) -> BookableSlot {
        let starts_at = Utc.with_ymd_and_hms(2026, 6, 2, hour, 0, 0).unwrap();
        BookableSlot {
            venue_id: crate::model::VenueId::new("zhs-munich"),
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

    fn disable_pruning_for_today(notifier: &DiscordNotifier) {
        *notifier.last_pruned.lock().unwrap() = Some(crate::time::today_berlin());
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
    async fn publish_never_prunes_the_rows_it_just_recorded() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
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

        assert_eq!(
            store
                .plan_strikes(&[BookableSlotId::from(&added)])
                .await
                .unwrap()
                .len(),
            1,
            "pruning ran before the post, not after it"
        );
        assert_eq!(
            *notifier.last_pruned.lock().unwrap(),
            Some(crate::time::today_berlin()),
            "pruning did run — the rows survived on merit, not by being skipped"
        );
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
        disable_pruning_for_today(&notifier);
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

    #[tokio::test]
    async fn the_edit_body_contains_the_struck_line() {
        let server = MockServer::start().await;
        let gone = slot("Court 2", 18);
        let struck = "~~Court 02: Tue, 02.06.2026 20:00–21:00~~";
        Mock::given(method("PATCH"))
            .and(path("/api/webhooks/123/token/messages/1408"))
            .and(body_string_contains(struck))
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

    #[tokio::test]
    async fn only_the_removed_line_is_struck() {
        let server = MockServer::start().await;
        let staying = slot("Court 1", 8);
        let gone = slot("Court 2", 18);
        Mock::given(method("PATCH"))
            .and(body_string_contains(
                "Court 01: Tue, 02.06.2026 10:00–11:00\\n~~",
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

    #[derive(Clone)]
    struct RateLimitOnce {
        calls: Arc<AtomicUsize>,
        then: ResponseTemplate,
    }

    impl RateLimitOnce {
        fn then(body: serde_json::Value) -> Self {
            Self {
                calls: Arc::default(),
                then: ResponseTemplate::new(200).set_body_json(body),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Respond for RateLimitOnce {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(429).set_body_json(serde_json::json!({ "retry_after": 0.0 }))
            } else {
                self.then.clone()
            }
        }
    }

    #[tokio::test]
    async fn a_rate_limited_post_is_retried_and_then_recorded() {
        let server = MockServer::start().await;
        let responder = RateLimitOnce::then(serde_json::json!({ "id": "1408" }));
        Mock::given(method("POST"))
            .respond_with(responder.clone())
            .expect(2)
            .mount(&server)
            .await;
        let (notifier, store) = notifier(&server).await;
        let added = slot("Court 2", 18);

        notifier
            .publish(&[AvailabilityChange::BecameBookable(added.clone())])
            .await
            .unwrap();

        assert_eq!(responder.calls(), 2, "one 429, then a retry");
        let plans = store
            .plan_strikes(&[BookableSlotId::from(&added)])
            .await
            .unwrap();
        assert_eq!(plans.len(), 1, "the retried post was recorded");
        assert_eq!(plans[0].message.id, "1408");
    }

    #[tokio::test]
    async fn a_trailing_slash_in_the_webhook_url_still_addresses_the_message() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/webhooks/123/token/messages/1408"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let store = Arc::new(SqliteStore::open_in_memory().await.unwrap());
        let url = format!("{}/api/webhooks/123/token/", server.uri())
            .parse()
            .unwrap();
        let notifier = DiscordNotifier::new(url, store.clone()).unwrap();
        let gone = slot("Court 2", 18);
        seed(&store, "1408", &gone).await;

        notifier
            .publish(&[AvailabilityChange::BecameUnbookable(gone)])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_rate_limited_edit_is_retried_and_then_committed() {
        let server = MockServer::start().await;
        let responder = RateLimitOnce::then(serde_json::json!({}));
        Mock::given(method("PATCH"))
            .respond_with(responder.clone())
            .expect(2)
            .mount(&server)
            .await;
        let (notifier, store) = notifier(&server).await;
        disable_pruning_for_today(&notifier);
        let gone = slot("Court 2", 18);
        seed(&store, "1408", &gone).await;

        notifier
            .publish(&[AvailabilityChange::BecameUnbookable(gone.clone())])
            .await
            .unwrap();

        assert_eq!(responder.calls(), 2, "one 429, then a retry");
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
        disable_pruning_for_today(&notifier);
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
            .expect(1)
            .mount(&server)
            .await;
        let (notifier, store) = notifier(&server).await;
        disable_pruning_for_today(&notifier);
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

    #[tokio::test]
    async fn an_unknown_webhook_does_not_delete_anything() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(404).set_body_json(unknown_webhook_body()))
            .mount(&server)
            .await;
        let (notifier, store) = notifier(&server).await;
        disable_pruning_for_today(&notifier);
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
        disable_pruning_for_today(&notifier);
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

    #[tokio::test]
    async fn an_already_started_slot_is_struck_before_pruning_can_drop_it() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/webhooks/123/token/messages/1408"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let (notifier, store) = notifier(&server).await;
        let started = slot("Court 2", 18);
        seed(&store, "1408", &started).await;

        notifier
            .publish(&[AvailabilityChange::BecameUnbookable(started.clone())])
            .await
            .unwrap();

        assert_eq!(
            store.prune_ended(Utc::now()).await.unwrap(),
            0,
            "publish already pruned the struck message"
        );
    }

    #[tokio::test]
    async fn pruning_runs_once_per_day() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "1408"
            })))
            .mount(&server)
            .await;
        let (notifier, _store) = notifier(&server).await;
        let added = slot("Court 2", 18);
        let changes = [AvailabilityChange::BecameBookable(added)];

        notifier.publish(&changes).await.unwrap();
        let after_first = *notifier.last_pruned.lock().unwrap();
        notifier.publish(&changes).await.unwrap();

        assert_eq!(after_first, Some(crate::time::today_berlin()));
        assert_eq!(*notifier.last_pruned.lock().unwrap(), after_first);
    }
}
