use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context as LayerContext;

const APPLICATION_TARGET: &str = "court_alert";
const DISCORD_MESSAGE_LIMIT: usize = 2_000;
const ERROR_QUEUE_CAPACITY: usize = 256;
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RATE_LIMIT_RETRIES: usize = 3;
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(1);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

pub struct DiscordErrorLayer {
    tx: mpsc::Sender<String>,
    dropped: AtomicU64,
}

pub struct DiscordErrorWorker {
    webhook_url: reqwest::Url,
    client: reqwest::Client,
    rx: mpsc::Receiver<String>,
}

impl DiscordErrorLayer {
    pub fn new(webhook_url: reqwest::Url) -> Result<(Self, DiscordErrorWorker)> {
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .context("building Discord error-webhook client")?;
        let (tx, rx) = mpsc::channel(ERROR_QUEUE_CAPACITY);
        Ok((
            Self {
                tx,
                dropped: AtomicU64::new(0),
            },
            DiscordErrorWorker {
                webhook_url,
                client,
                rx,
            },
        ))
    }
}

impl<S> Layer<S> for DiscordErrorLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: LayerContext<'_, S>) {
        let metadata = event.metadata();
        if !is_application_target(metadata.target()) {
            return;
        }

        let mut visitor = ErrorEventVisitor::default();
        event.record(&mut visitor);
        let should_forward = metadata.level() == &Level::ERROR
            || (metadata.level() == &Level::WARN && visitor.has_error);
        if !should_forward {
            return;
        }

        let message = visitor
            .message
            .unwrap_or_else(|| metadata.name().to_string());
        let mut rendered = format!(
            "**{}** `{}` — {}",
            metadata.level(),
            metadata.target(),
            message
        );
        for (name, value) in visitor.fields {
            let _ = write!(rendered, "\n**{name}:** {value}");
        }
        let rendered = redact_discord_webhook_tokens(&rendered)
            .chars()
            .take(DISCORD_MESSAGE_LIMIT)
            .collect::<String>();

        if self.tx.try_send(rendered).is_err() {
            let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped == 1 || dropped.is_power_of_two() {
                eprintln!(
                    "Discord monitoring queue full or closed; dropped {dropped} error event(s)"
                );
            }
        }
    }
}

impl DiscordErrorWorker {
    pub async fn run(mut self) {
        let mut pending = None;
        loop {
            let first = match pending.take() {
                Some(message) => message,
                None => match self.rx.recv().await {
                    Some(message) => message,
                    None => break,
                },
            };
            let (content, overflow) = batch_queued_messages(first, &mut self.rx);
            pending = overflow;

            if let Err(error) = self.send(&content).await {
                eprintln!("failed to post error to Discord monitoring webhook: {error:#}");
            }
        }
    }

    async fn send(&self, content: &str) -> Result<()> {
        #[derive(Serialize)]
        struct AllowedMentions<'a> {
            parse: [&'a str; 0],
        }

        #[derive(Serialize)]
        struct Body<'a> {
            content: &'a str,
            allowed_mentions: AllowedMentions<'a>,
        }

        #[derive(Deserialize)]
        struct RateLimitBody {
            retry_after: Option<f64>,
        }

        for attempt in 0..=MAX_RATE_LIMIT_RETRIES {
            let response = self
                .client
                .post(self.webhook_url.clone())
                .json(&Body {
                    content,
                    allowed_mentions: AllowedMentions { parse: [] },
                })
                .send()
                .await
                .map_err(reqwest::Error::without_url)
                .context("posting error to Discord monitoring webhook")?;
            let status = response.status();
            let header_delay = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_retry_after);
            let body = response.text().await.unwrap_or_default();

            if status.is_success() {
                return Ok(());
            }
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt < MAX_RATE_LIMIT_RETRIES
            {
                let body_delay = serde_json::from_str::<RateLimitBody>(&body)
                    .ok()
                    .and_then(|body| body.retry_after)
                    .and_then(duration_from_seconds);
                let delay = header_delay
                    .or(body_delay)
                    .unwrap_or(DEFAULT_RETRY_AFTER)
                    .min(MAX_RETRY_AFTER);
                tokio::time::sleep(delay).await;
                continue;
            }

            let body = redact_discord_webhook_tokens(&body);
            anyhow::bail!("Discord monitoring webhook returned {status}: {body}");
        }
        unreachable!("rate-limit retry loop always returns or fails")
    }
}

fn batch_queued_messages(
    first: String,
    rx: &mut mpsc::Receiver<String>,
) -> (String, Option<String>) {
    let mut batch = first;
    let mut chars = batch.chars().count();
    loop {
        let next = match rx.try_recv() {
            Ok(message) => message,
            Err(_) => return (batch, None),
        };
        let next_chars = next.chars().count();
        if chars + 2 + next_chars > DISCORD_MESSAGE_LIMIT {
            return (batch, Some(next));
        }
        batch.push_str("\n\n");
        batch.push_str(&next);
        chars += 2 + next_chars;
    }
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse().ok().and_then(duration_from_seconds)
}

fn duration_from_seconds(seconds: f64) -> Option<Duration> {
    (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds))
}

fn is_application_target(target: &str) -> bool {
    target == APPLICATION_TARGET
        || target
            .strip_prefix(APPLICATION_TARGET)
            .is_some_and(|suffix| suffix.starts_with("::"))
}

fn redact_discord_webhook_tokens(input: &str) -> String {
    const MARKER: &str = "/api/webhooks/";
    const REDACTED: &str = "[REDACTED]";

    let mut output = input.to_string();
    let mut search_from = 0;
    while let Some(marker_offset) = output[search_from..].find(MARKER) {
        let id_start = search_from + marker_offset + MARKER.len();
        let Some(id_end_offset) = output[id_start..].find('/') else {
            break;
        };
        let token_start = id_start + id_end_offset + 1;
        let token_len = output[token_start..]
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
            .map(char::len_utf8)
            .sum::<usize>();
        if token_len == 0 {
            search_from = token_start;
            continue;
        }
        output.replace_range(token_start..token_start + token_len, REDACTED);
        search_from = token_start + REDACTED.len();
    }
    output
}

#[derive(Default)]
struct ErrorEventVisitor {
    message: Option<String>,
    fields: Vec<(String, String)>,
    has_error: bool,
}

impl ErrorEventVisitor {
    fn record(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = Some(value);
        } else {
            if field.name() == "error" {
                self.has_error = true;
            }
            self.fields.push((field.name().to_string(), value));
        }
    }
}

impl Visit for ErrorEventVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record(field, format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tracing_subscriber::layer::SubscriberExt as _;
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn layer() -> (DiscordErrorLayer, DiscordErrorWorker) {
        DiscordErrorLayer::new("https://discord.com/api/webhooks/1/token".parse().unwrap()).unwrap()
    }

    #[test]
    fn forwards_only_application_errors_and_warnings_with_error_fields() {
        let (layer, mut worker) = layer();
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(error = "not an error event", "healthy");
            tracing::warn!("ordinary warning");
            tracing::warn!(error = "request failed", "tick failed");
            tracing::error!(reason = "broken", "provider failed");
            tracing::error!(target: "reqwest", "dependency failed");
        });

        let warning = worker.rx.try_recv().unwrap();
        assert!(warning.contains("tick failed"));
        assert!(warning.contains("**error:** request failed"));
        assert!(worker.rx.try_recv().unwrap().contains("provider failed"));
        assert!(worker.rx.try_recv().is_err());
    }

    #[test]
    fn redacts_discord_webhook_tokens() {
        let input = "request failed for url (https://discord.com/api/webhooks/123/secret.Token-_)";
        assert_eq!(
            redact_discord_webhook_tokens(input),
            "request failed for url (https://discord.com/api/webhooks/123/[REDACTED])"
        );
    }

    #[test]
    fn rendered_events_do_not_exceed_discords_limit() {
        let (layer, mut worker) = layer();
        let subscriber = tracing_subscriber::registry().with(layer);
        let long_error = "ä".repeat(DISCORD_MESSAGE_LIMIT * 2);

        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(error = long_error, "failed");
        });

        assert_eq!(
            worker.rx.try_recv().unwrap().chars().count(),
            DISCORD_MESSAGE_LIMIT
        );
    }

    #[test]
    fn queued_events_are_batched_without_exceeding_the_limit() {
        let (tx, mut rx) = mpsc::channel(2);
        tx.try_send("first".to_string()).unwrap();
        tx.try_send("second".to_string()).unwrap();
        let first = rx.try_recv().unwrap();

        let (batch, overflow) = batch_queued_messages(first, &mut rx);

        assert_eq!(batch, "first\n\nsecond");
        assert!(overflow.is_none());
    }

    #[tokio::test]
    async fn worker_posts_with_mentions_disabled() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(serde_json::json!({
                "content": "test error",
                "allowed_mentions": { "parse": [] }
            })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let (_layer, worker) =
            DiscordErrorLayer::new(format!("{}/webhook", server.uri()).parse().unwrap()).unwrap();

        worker.send("test error").await.unwrap();
    }

    #[derive(Clone, Default)]
    struct RateLimitOnce(Arc<AtomicUsize>);

    impl Respond for RateLimitOnce {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(429).set_body_json(serde_json::json!({ "retry_after": 0.0 }))
            } else {
                ResponseTemplate::new(204)
            }
        }
    }

    #[tokio::test]
    async fn worker_retries_after_rate_limit_response() {
        let server = MockServer::start().await;
        let responder = RateLimitOnce::default();
        Mock::given(method("POST"))
            .respond_with(responder.clone())
            .expect(2)
            .mount(&server)
            .await;
        let (_layer, worker) =
            DiscordErrorLayer::new(format!("{}/webhook", server.uri()).parse().unwrap()).unwrap();

        worker.send("test error").await.unwrap();

        assert_eq!(responder.0.load(Ordering::SeqCst), 2);
    }
}
