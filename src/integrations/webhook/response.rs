use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

pub(super) fn error(status: StatusCode, kind: &str) -> Response {
    (status, Json(json!({ "error": kind }))).into_response()
}

pub(super) fn invalid_request(message: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "invalid_request", "message": message })),
    )
        .into_response()
}

pub(super) fn rejected(body: Value) -> Response {
    (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response()
}
