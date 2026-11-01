use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::Response;
use http_body_util::LengthLimitError;

use super::response::{error, invalid_request};
use super::signing;

const TIMESTAMP_HEADER: &str = "x-webhook-timestamp";
const SIGNATURE_HEADER: &str = "x-webhook-signature-v2";
const MAX_CLOCK_SKEW_SECS: u64 = 300;
const MAX_BODY_BYTES: usize = 64 * 1024;
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) async fn require_signature(
    State(secret): State<Arc<str>>,
    request: Request,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();
    let Some(claim) = SignatureClaim::from_headers(&parts, chrono::Utc::now().timestamp()) else {
        return unauthorized();
    };
    let body = match tokio::time::timeout(
        BODY_READ_TIMEOUT,
        axum::body::to_bytes(body, MAX_BODY_BYTES),
    )
    .await
    {
        Ok(Ok(body)) => body,
        Ok(Err(e)) if is_over_limit(&e) => {
            return error(StatusCode::PAYLOAD_TOO_LARGE, "body_too_large");
        }
        Ok(Err(_)) => return invalid_request("the request body could not be read".into()),
        Err(_) => return error(StatusCode::REQUEST_TIMEOUT, "request_timeout"),
    };
    if !claim.matches(&secret, &parts, &body) {
        return unauthorized();
    }
    next.run(Request::from_parts(parts, Body::from(body))).await
}

fn unauthorized() -> Response {
    error(StatusCode::UNAUTHORIZED, "unauthorized")
}

fn is_over_limit(e: &axum::Error) -> bool {
    std::error::Error::source(e).is_some_and(|source| source.is::<LengthLimitError>())
}

struct SignatureClaim<'a> {
    timestamp: &'a str,
    signature: Vec<u8>,
}

impl<'a> SignatureClaim<'a> {
    fn from_headers(parts: &'a Parts, now: i64) -> Option<Self> {
        let header = |name| {
            parts
                .headers
                .get(name)
                .and_then(|value| value.to_str().ok())
        };
        let timestamp = header(TIMESTAMP_HEADER)?;
        let sent_at = timestamp.parse::<i64>().ok()?;
        if now.abs_diff(sent_at) > MAX_CLOCK_SKEW_SECS {
            return None;
        }
        let signature = hex::decode(header(SIGNATURE_HEADER)?).ok()?;
        Some(Self {
            timestamp,
            signature,
        })
    }

    fn matches(&self, secret: &str, parts: &Parts, body: &[u8]) -> bool {
        let path = parts
            .uri
            .path_and_query()
            .map_or(parts.uri.path(), |path| path.as_str());
        signing::verify(
            secret,
            &[
                self.timestamp.as_bytes(),
                parts.method.as_str().as_bytes(),
                path.as_bytes(),
                body,
            ],
            &self.signature,
        )
    }
}
