use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
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

pub(super) struct InvalidRequest(String);

impl IntoResponse for InvalidRequest {
    fn into_response(self) -> Response {
        invalid_request(self.0)
    }
}

impl From<JsonRejection> for InvalidRequest {
    fn from(rejection: JsonRejection) -> Self {
        Self(rejection.body_text())
    }
}

impl From<QueryRejection> for InvalidRequest {
    fn from(rejection: QueryRejection) -> Self {
        Self(rejection.body_text())
    }
}

impl From<PathRejection> for InvalidRequest {
    fn from(rejection: PathRejection) -> Self {
        Self(rejection.body_text())
    }
}

pub(super) fn rejected(body: Value) -> Response {
    (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response()
}
