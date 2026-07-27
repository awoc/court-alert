//! HTTP details shared by the two Discord webhook clients: rate-limit backoff
//! and keeping the webhook token out of logs.

use std::time::Duration;

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

    #[test]
    fn redacts_discord_webhook_tokens_in_error_bodies() {
        let input = "posting to https://discord.com/api/webhooks/123/abcDEF-_. failed";
        let redacted = redact_discord_webhook_tokens(input);
        assert!(redacted.contains("/api/webhooks/123/[REDACTED]"));
        assert!(!redacted.contains("abcDEF"));
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
