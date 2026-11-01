use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::Response;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use super::response::error;

const TIMESTAMP_HEADER: &str = "x-webhook-timestamp";
const SIGNATURE_HEADER: &str = "x-webhook-signature-v2";
const MAX_CLOCK_SKEW_SECS: u64 = 300;
const MAX_BODY_BYTES: usize = 64 * 1024;

pub(super) async fn require_signature(
    State(secret): State<Arc<str>>,
    request: Request,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();
    let Ok(body) = axum::body::to_bytes(body, MAX_BODY_BYTES).await else {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "body_too_large");
    };
    if !is_signed(&secret, &parts, &body, chrono::Utc::now().timestamp()) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    next.run(Request::from_parts(parts, Body::from(body))).await
}

fn is_signed(secret: &str, parts: &Parts, body: &[u8], now: i64) -> bool {
    let header = |name| {
        parts
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
    };
    let (Some(timestamp), Some(signature)) = (header(TIMESTAMP_HEADER), header(SIGNATURE_HEADER))
    else {
        return false;
    };
    let Ok(sent_at) = timestamp.parse::<i64>() else {
        return false;
    };
    if now.abs_diff(sent_at) > MAX_CLOCK_SKEW_SECS {
        return false;
    }
    let Ok(signature) = hex::decode(signature) else {
        return false;
    };
    let path = parts
        .uri
        .path_and_query()
        .map_or(parts.uri.path(), |path| path.as_str());

    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    for part in [
        timestamp.as_bytes(),
        parts.method.as_str().as_bytes(),
        path.as_bytes(),
    ] {
        mac.update(part);
        mac.update(b".");
    }
    mac.update(body);
    mac.verify_slice(&signature).is_ok()
}
