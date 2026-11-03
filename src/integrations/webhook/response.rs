use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

use crate::subscriptions::contract::{CourtChoice, SubscriptionRejection};

#[derive(Serialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub(super) enum ApiError {
    InvalidRequest {
        message: String,
    },
    Unauthorized,
    NotFound,
    RequestTimeout,
    BodyTooLarge,
    Internal,
    InvalidSubscriberId,
    InvalidTimeRange,
    InvalidSchedule,
    TooManyCourts {
        max: usize,
    },
    CourtNameTooLong {
        max: usize,
    },
    UnknownCourts {
        unknown: RejectedCourts,
        available: Vec<Alternative<Uuid>>,
    },
    CourtsNeedClub,
    CourtCatalogsNotReady,
    UnknownClub {
        unknown: String,
        available: Vec<Alternative<String>>,
    },
    NoClubsConfigured {
        sport: &'static str,
    },
    FilterExcludesCourts {
        courts: Vec<String>,
        filter: &'static str,
    },
    FilterNotForSport {
        sport: &'static str,
        filter: &'static str,
    },
}

#[derive(Serialize)]
#[serde(untagged)]
pub(super) enum RejectedCourts {
    Names(Vec<String>),
    Ids(Vec<Uuid>),
}

#[derive(Serialize)]
pub(super) struct Alternative<Id> {
    id: Id,
    name: String,
}

impl ApiError {
    pub(super) fn invalid_request(message: impl Into<String>) -> Self {
        Self::InvalidRequest {
            message: message.into(),
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::InvalidRequest { .. } => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::RequestTimeout => StatusCode::REQUEST_TIMEOUT,
            Self::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            Self::InvalidSubscriberId
            | Self::InvalidTimeRange
            | Self::InvalidSchedule
            | Self::TooManyCourts { .. }
            | Self::CourtNameTooLong { .. }
            | Self::UnknownCourts { .. }
            | Self::CourtsNeedClub
            | Self::CourtCatalogsNotReady
            | Self::UnknownClub { .. }
            | Self::NoClubsConfigured { .. }
            | Self::FilterExcludesCourts { .. }
            | Self::FilterNotForSport { .. } => StatusCode::UNPROCESSABLE_ENTITY,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status(), Json(json!(self))).into_response()
    }
}

impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        Self::invalid_request(rejection.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(rejection: QueryRejection) -> Self {
        Self::invalid_request(rejection.body_text())
    }
}

impl From<PathRejection> for ApiError {
    fn from(rejection: PathRejection) -> Self {
        Self::invalid_request(rejection.body_text())
    }
}

impl From<SubscriptionRejection> for ApiError {
    fn from(rejection: SubscriptionRejection) -> Self {
        match rejection {
            SubscriptionRejection::InvalidTimeRange => Self::InvalidTimeRange,
            SubscriptionRejection::InvalidSchedule => Self::InvalidSchedule,
            SubscriptionRejection::TooManyCourts { max } => Self::TooManyCourts { max },
            SubscriptionRejection::CourtNameTooLong { max } => Self::CourtNameTooLong { max },
            SubscriptionRejection::UnknownCourts { unknown, available } => Self::UnknownCourts {
                unknown: match unknown {
                    CourtChoice::Names(names) => RejectedCourts::Names(names),
                    CourtChoice::Ids(ids) => RejectedCourts::Ids(ids),
                },
                available: available
                    .iter()
                    .map(|court| Alternative {
                        id: court.id(),
                        name: court.name().to_owned(),
                    })
                    .collect(),
            },
            SubscriptionRejection::CourtsNeedClub => Self::CourtsNeedClub,
            SubscriptionRejection::CourtCatalogsNotReady => Self::CourtCatalogsNotReady,
            SubscriptionRejection::UnknownClub { unknown, available } => Self::UnknownClub {
                unknown: unknown.as_str().to_owned(),
                available: available
                    .into_iter()
                    .map(|club| Alternative {
                        id: club.venue_id.as_str().to_owned(),
                        name: club.name,
                    })
                    .collect(),
            },
            SubscriptionRejection::NoClubsConfigured { sport } => Self::NoClubsConfigured {
                sport: sport.as_str(),
            },
            SubscriptionRejection::FilterExcludesCourts { courts, filter } => {
                Self::FilterExcludesCourts {
                    courts,
                    filter: filter.as_str(),
                }
            }
            SubscriptionRejection::FilterNotForSport { sport, filter } => Self::FilterNotForSport {
                sport: sport.as_str(),
                filter: filter.as_str(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        ClubRef, Court, CourtAttributes, CourtFilter, CourtLocation, CourtSurface, Sport, VenueId,
    };

    const COURT_ONE: &str = r#"{"id":"00000000-0000-0000-0000-000000000001","name":"Court 1"}"#;

    fn courts() -> Vec<Court> {
        vec![Court::new(
            Uuid::from_u128(1),
            "Court 1".into(),
            CourtAttributes::tennis(CourtSurface::Clay),
        )]
    }

    async fn assert_responds(error: impl Into<ApiError>, status: StatusCode, body: &str) {
        let response = error.into().into_response();
        assert_eq!(response.status(), status, "{body}");
        assert_eq!(response.headers()["content-type"], "application/json");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&bytes), body);
    }

    #[tokio::test]
    async fn every_request_error_has_its_exact_status_and_body() {
        for (error, status, body) in [
            (
                ApiError::invalid_request("nope"),
                StatusCode::BAD_REQUEST,
                r#"{"error":"invalid_request","message":"nope"}"#,
            ),
            (
                ApiError::Unauthorized,
                StatusCode::UNAUTHORIZED,
                r#"{"error":"unauthorized"}"#,
            ),
            (
                ApiError::NotFound,
                StatusCode::NOT_FOUND,
                r#"{"error":"not_found"}"#,
            ),
            (
                ApiError::RequestTimeout,
                StatusCode::REQUEST_TIMEOUT,
                r#"{"error":"request_timeout"}"#,
            ),
            (
                ApiError::BodyTooLarge,
                StatusCode::PAYLOAD_TOO_LARGE,
                r#"{"error":"body_too_large"}"#,
            ),
            (
                ApiError::Internal,
                StatusCode::INTERNAL_SERVER_ERROR,
                r#"{"error":"internal"}"#,
            ),
            (
                ApiError::InvalidSubscriberId,
                StatusCode::UNPROCESSABLE_ENTITY,
                r#"{"error":"invalid_subscriber_id"}"#,
            ),
        ] {
            assert_responds(error, status, body).await;
        }
    }

    #[tokio::test]
    async fn every_subscription_rejection_has_its_exact_status_and_body() {
        for (rejection, body) in [
            (
                SubscriptionRejection::NoClubsConfigured {
                    sport: Sport::Tennis,
                },
                r#"{"error":"no_clubs_configured","sport":"tennis"}"#.to_owned(),
            ),
            (
                SubscriptionRejection::UnknownClub {
                    unknown: VenueId::new("x"),
                    available: vec![ClubRef {
                        venue_id: VenueId::new("zhs"),
                        name: "ZHS".into(),
                    }],
                },
                r#"{"available":[{"id":"zhs","name":"ZHS"}],"error":"unknown_club","unknown":"x"}"#
                    .to_owned(),
            ),
            (
                SubscriptionRejection::InvalidTimeRange,
                r#"{"error":"invalid_time_range"}"#.to_owned(),
            ),
            (
                SubscriptionRejection::InvalidSchedule,
                r#"{"error":"invalid_schedule"}"#.to_owned(),
            ),
            (
                SubscriptionRejection::FilterNotForSport {
                    sport: Sport::Tennis,
                    filter: CourtFilter::Location(CourtLocation::Indoor),
                },
                r#"{"error":"filter_not_for_sport","filter":"indoor","sport":"tennis"}"#.to_owned(),
            ),
            (
                SubscriptionRejection::TooManyCourts { max: 20 },
                r#"{"error":"too_many_courts","max":20}"#.to_owned(),
            ),
            (
                SubscriptionRejection::CourtNameTooLong { max: 100 },
                r#"{"error":"court_name_too_long","max":100}"#.to_owned(),
            ),
            (
                SubscriptionRejection::CourtCatalogsNotReady,
                r#"{"error":"court_catalogs_not_ready"}"#.to_owned(),
            ),
            (
                SubscriptionRejection::CourtsNeedClub,
                r#"{"error":"courts_need_club"}"#.to_owned(),
            ),
            (
                SubscriptionRejection::UnknownCourts {
                    unknown: CourtChoice::Names(vec!["Nope".into()]),
                    available: courts(),
                },
                format!(
                    r#"{{"available":[{COURT_ONE}],"error":"unknown_courts","unknown":["Nope"]}}"#
                ),
            ),
            (
                SubscriptionRejection::UnknownCourts {
                    unknown: CourtChoice::Ids(vec![Uuid::from_u128(2)]),
                    available: courts(),
                },
                format!(
                    r#"{{"available":[{COURT_ONE}],"error":"unknown_courts","unknown":["00000000-0000-0000-0000-000000000002"]}}"#
                ),
            ),
            (
                SubscriptionRejection::FilterExcludesCourts {
                    courts: vec!["Court 1".into()],
                    filter: CourtFilter::CLAY,
                },
                r#"{"courts":["Court 1"],"error":"filter_excludes_courts","filter":"clay"}"#
                    .to_owned(),
            ),
        ] {
            assert_responds(rejection, StatusCode::UNPROCESSABLE_ENTITY, &body).await;
        }
    }
}
