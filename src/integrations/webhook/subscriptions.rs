use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{NaiveDate, Weekday};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::error;

use super::INTEGRATION_NAME;
use super::response::{error, invalid_request, rejected};
use crate::model::{CourtFilter, MINUTES_PER_DAY, ProviderUserRef, Schedule, Sport, VenueId};
use crate::subscriptions::SubscriptionService;
use crate::subscriptions::contract::{
    AvailableSlotSummary, SubscriptionCommand, SubscriptionResult, SubscriptionSummary,
};
use crate::time::fmt_hhmm;

const MAX_USER_ID_CHARS: usize = 128;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SubscribeRequest {
    user_id: String,
    sport: Sport,
    club: Option<VenueId>,
    weekday: Option<Weekday>,
    date: Option<NaiveDate>,
    start: String,
    end: String,
    courts: Option<Vec<String>>,
    filter: Option<CourtFilter>,
}

#[derive(Deserialize)]
pub(super) struct UserQuery {
    user: String,
}

#[derive(Serialize)]
struct Subscription {
    id: i64,
    sport: &'static str,
    club: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    weekday: Option<Weekday>,
    #[serde(skip_serializing_if = "Option::is_none")]
    date: Option<NaiveDate>,
    start: String,
    end: String,
    courts: Option<Vec<String>>,
    filter: &'static str,
}

impl From<SubscriptionSummary> for Subscription {
    fn from(summary: SubscriptionSummary) -> Self {
        let (weekday, date) = match summary.schedule {
            Schedule::Weekday(weekday) => (Some(weekday), None),
            Schedule::Date(date) => (None, Some(date)),
        };
        Self {
            id: summary.id,
            sport: summary.sport.as_str(),
            club: summary.club,
            weekday,
            date,
            start: fmt_hhmm(summary.time_range.start_minute()),
            end: fmt_hhmm(summary.time_range.end_minute()),
            courts: summary.courts,
            filter: summary.filter.as_str(),
        }
    }
}

#[derive(Serialize)]
struct OpenSlot {
    club: String,
    court: String,
    court_id: uuid::Uuid,
    starts_at: chrono::DateTime<chrono::Utc>,
    ends_at: chrono::DateTime<chrono::Utc>,
}

impl From<AvailableSlotSummary> for OpenSlot {
    fn from(slot: AvailableSlotSummary) -> Self {
        Self {
            club: slot.club,
            court: slot.court,
            court_id: slot.court_id,
            starts_at: slot.starts_at,
            ends_at: slot.ends_at,
        }
    }
}

pub(super) async fn subscribe(
    State(service): State<Arc<SubscriptionService>>,
    request: Result<Json<SubscribeRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match request {
        Ok(request) => request,
        Err(rejection) => return invalid_request(rejection.body_text()),
    };
    let schedule = match (request.weekday, request.date) {
        (Some(weekday), None) => Schedule::Weekday(weekday),
        (None, Some(date)) => Schedule::Date(date),
        _ => return invalid_request("exactly one of weekday or date is required".into()),
    };
    let (Some(start_minute), Some(end_minute)) =
        (minute_of_day(&request.start), minute_of_day(&request.end))
    else {
        return invalid_request("start and end must be times of day as HH:MM".into());
    };
    let command = SubscriptionCommand::Subscribe {
        sport: request.sport,
        venue: request.club,
        schedule,
        start_minute,
        end_minute,
        courts: request.courts,
        filter: request.filter,
    };
    handle(&service, &request.user_id, command).await
}

pub(super) async fn list(
    State(service): State<Arc<SubscriptionService>>,
    query: Result<Query<UserQuery>, QueryRejection>,
) -> Response {
    match query {
        Ok(Query(query)) => handle(&service, &query.user, SubscriptionCommand::List).await,
        Err(rejection) => invalid_request(rejection.body_text()),
    }
}

pub(super) async fn unsubscribe(
    State(service): State<Arc<SubscriptionService>>,
    id: Result<Path<i64>, PathRejection>,
    query: Result<Query<UserQuery>, QueryRejection>,
) -> Response {
    let Path(id) = match id {
        Ok(id) => id,
        Err(rejection) => return invalid_request(rejection.body_text()),
    };
    match query {
        Ok(Query(query)) => {
            handle(
                &service,
                &query.user,
                SubscriptionCommand::Unsubscribe { id },
            )
            .await
        }
        Err(rejection) => invalid_request(rejection.body_text()),
    }
}

fn minute_of_day(raw: &str) -> Option<u32> {
    let (hours, minutes) = raw.split_once(':')?;
    if hours.len() != 2 || minutes.len() != 2 {
        return None;
    }
    let (hours, minutes): (u32, u32) = (hours.parse().ok()?, minutes.parse().ok()?);
    let minute = hours * 60 + minutes;
    (minutes < 60 && minute <= MINUTES_PER_DAY).then_some(minute)
}

async fn handle(
    service: &SubscriptionService,
    user_id: &str,
    command: SubscriptionCommand,
) -> Response {
    if user_id.is_empty() || user_id.chars().count() > MAX_USER_ID_CHARS {
        return rejected(json!({ "error": "invalid_user_id" }));
    }
    let user = ProviderUserRef {
        provider: INTEGRATION_NAME.to_owned(),
        user_id: user_id.to_owned(),
    };
    match service.handle(&user, command).await {
        Ok(result) => respond(result),
        Err(e) => {
            error!(error = %format!("{e:#}"), "webhook API subscription command failed");
            internal_error()
        }
    }
}

fn respond(result: SubscriptionResult) -> Response {
    match result {
        SubscriptionResult::Subscribed {
            summary,
            open_slots,
        } => (
            StatusCode::CREATED,
            Json(json!({
                "subscription": Subscription::from(summary),
                "open_slots": open_slots.into_iter().map(OpenSlot::from).collect::<Vec<_>>(),
            })),
        )
            .into_response(),
        SubscriptionResult::SubscriptionList(subscriptions) => Json(json!({
            "subscriptions": subscriptions
                .into_iter()
                .map(Subscription::from)
                .collect::<Vec<_>>(),
        }))
        .into_response(),
        SubscriptionResult::Unsubscribed { .. } => StatusCode::NO_CONTENT.into_response(),
        SubscriptionResult::NotFound { .. } => error(StatusCode::NOT_FOUND, "not_found"),
        SubscriptionResult::InvalidTimeRange => rejected(json!({ "error": "invalid_time_range" })),
        SubscriptionResult::InvalidSchedule => rejected(json!({ "error": "invalid_schedule" })),
        SubscriptionResult::UnknownCourts { unknown, available } => rejected(json!({
            "error": "unknown_courts",
            "unknown": unknown,
            "available": available,
        })),
        SubscriptionResult::UnknownClub { unknown, available } => rejected(json!({
            "error": "unknown_club",
            "unknown": unknown,
            "available": available,
        })),
        SubscriptionResult::NoClubsConfigured { sport } => rejected(json!({
            "error": "no_clubs_configured",
            "sport": sport.as_str(),
        })),
        SubscriptionResult::FilterExcludesCourts { courts, filter } => rejected(json!({
            "error": "filter_excludes_courts",
            "courts": courts,
            "filter": filter.as_str(),
        })),
        SubscriptionResult::AllSubscriptions(_) | SubscriptionResult::NotAuthorized => {
            error!("webhook API received a result for an admin command it never sends");
            internal_error()
        }
    }
}

fn internal_error() -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal")
}
