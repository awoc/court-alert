//! HTTP details shared by the two Discord webhook clients: rate-limit backoff
//! and keeping the webhook token out of logs.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

pub(super) const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
pub(super) const MAX_RATE_LIMIT_RETRIES: usize = 3;
pub(super) const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(1);
pub(super) const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

pub(super) fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse().ok().and_then(duration_from_seconds)
}

/// Rejects negative, NaN, infinite, and — unlike a bare `is_finite()` guard —
/// finite values too large for a `Duration`. `Duration::from_secs_f64` panics
/// on those, and the value comes from a `Retry-After` header we do not control.
pub(super) fn duration_from_seconds(seconds: f64) -> Option<Duration> {
    Duration::try_from_secs_f64(seconds).ok()
}

/// How long to wait before a rate-limited retry. Discord sends the delay in a
/// `Retry-After` header and again in the JSON body; the header wins, and both
/// are capped so a hostile or buggy value cannot stall a poll tick.
pub(super) fn retry_delay(header: Option<Duration>, body: &str) -> Duration {
    #[derive(Deserialize)]
    struct RateLimitBody {
        retry_after: Option<f64>,
    }

    let body_delay = serde_json::from_str::<RateLimitBody>(body)
        .ok()
        .and_then(|body| body.retry_after)
        .and_then(duration_from_seconds);
    header
        .or(body_delay)
        .unwrap_or(DEFAULT_RETRY_AFTER)
        .min(MAX_RETRY_AFTER)
}

/// Sends a request, resending it while Discord answers 429, and returns the
/// final status and body for the caller to interpret — a 404 means different
/// things to a POST and to a PATCH, so only the rate-limit handling is shared.
///
/// `request` is called afresh per attempt because a `RequestBuilder` cannot be
/// cloned once it carries a body. Only the response is retried, never a
/// transport error: a timeout leaves us unable to say whether Discord acted on
/// the request, and resending a POST on that would double-alert the channel.
pub(super) async fn send_with_rate_limit_retry(
    request: impl Fn() -> reqwest::RequestBuilder,
    context: &'static str,
) -> Result<(reqwest::StatusCode, String)> {
    let mut attempt = 0;
    loop {
        let response = request()
            .send()
            .await
            // Carries the webhook URL, token and all, into any log line.
            .map_err(reqwest::Error::without_url)
            .context(context)?;
        let status = response.status();
        let header_delay = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_retry_after);
        let body = response.text().await.unwrap_or_default();

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt < MAX_RATE_LIMIT_RETRIES {
            attempt += 1;
            tokio::time::sleep(retry_delay(header_delay, &body)).await;
            continue;
        }
        return Ok((status, body));
    }
}

pub(super) fn redact_discord_webhook_tokens(input: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserted on the whole string rather than with `contains`: the point of
    /// this function is that nothing of the token survives, so a regression
    /// that swallowed the trailing text or left a token suffix behind has to
    /// fail here.
    #[test]
    fn redacts_discord_webhook_tokens_in_error_bodies() {
        let input = "posting to https://discord.com/api/webhooks/123/abcDEF-_. failed";
        assert_eq!(
            redact_discord_webhook_tokens(input),
            "posting to https://discord.com/api/webhooks/123/[REDACTED] failed"
        );
    }

    /// The scan resumes *after* the replacement, so a second URL later in the
    /// same message is redacted too — a batched error report carries several.
    #[test]
    fn redacts_every_token_in_a_message() {
        let input = "https://discord.com/api/webhooks/1/first and \
                     https://discord.com/api/webhooks/2/second";
        assert_eq!(
            redact_discord_webhook_tokens(input),
            "https://discord.com/api/webhooks/1/[REDACTED] and \
             https://discord.com/api/webhooks/2/[REDACTED]"
        );
    }

    #[test]
    fn retry_delay_prefers_the_header_over_the_body() {
        let delay = retry_delay(Some(Duration::from_secs(2)), r#"{"retry_after":9.0}"#);
        assert_eq!(delay, Duration::from_secs(2));
    }

    #[test]
    fn retry_delay_falls_back_to_the_body_then_to_the_default() {
        assert_eq!(
            retry_delay(None, r#"{"retry_after":2.5}"#),
            Duration::from_millis(2500)
        );
        assert_eq!(retry_delay(None, "not json"), DEFAULT_RETRY_AFTER);
    }

    #[test]
    fn retry_delay_is_capped() {
        assert_eq!(
            retry_delay(Some(Duration::from_secs(9999)), ""),
            MAX_RETRY_AFTER
        );
    }

    #[test]
    fn negative_and_infinite_delays_are_rejected() {
        assert!(duration_from_seconds(-1.0).is_none());
        assert!(duration_from_seconds(f64::INFINITY).is_none());
        assert!(duration_from_seconds(f64::NAN).is_none());
        assert!(parse_retry_after("  1.5 ").is_some());
        assert!(parse_retry_after("soon").is_none());
    }

    /// `1e300` is finite and positive, so an `is_finite() && >= 0.0` guard lets
    /// it through — and `Duration::from_secs_f64` then panics on it. The cap in
    /// `retry_delay` cannot help, because it applies after the conversion.
    #[test]
    fn a_finite_but_enormous_delay_is_rejected_rather_than_panicking() {
        assert!(duration_from_seconds(1e300).is_none());
        assert_eq!(
            retry_delay(None, r#"{"retry_after":1e300}"#),
            DEFAULT_RETRY_AFTER
        );
        assert_eq!(
            retry_delay(parse_retry_after("1e300"), ""),
            DEFAULT_RETRY_AFTER
        );
    }
}
