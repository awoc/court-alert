use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::Response;
use http_body_util::LengthLimitError;

use super::response::ApiError;
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
) -> Result<Response, ApiError> {
    let (parts, body) = request.into_parts();
    let claim = SignatureClaim::from_headers(&parts, chrono::Utc::now().timestamp())
        .ok_or(ApiError::Unauthorized)?;
    let body = match tokio::time::timeout(
        BODY_READ_TIMEOUT,
        axum::body::to_bytes(body, MAX_BODY_BYTES),
    )
    .await
    {
        Ok(Ok(body)) => body,
        Ok(Err(e)) if is_over_limit(&e) => return Err(ApiError::BodyTooLarge),
        Ok(Err(_)) => {
            return Err(ApiError::invalid_request(
                "the request body could not be read",
            ));
        }
        Err(_) => return Err(ApiError::RequestTimeout),
    };
    if !claim.matches(&secret, &parts, &body) {
        return Err(ApiError::Unauthorized);
    }
    Ok(next.run(Request::from_parts(parts, Body::from(body))).await)
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
        signing::verify_request(
            secret,
            self.timestamp,
            parts.method.as_str(),
            path,
            body,
            &self.signature,
        )
    }
}
