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

use super::INTEGRATION;
use super::response::{InvalidRequest, error, invalid_request, rejected};
use super::slot::Slot;
use crate::model::{CourtFilter, MINUTES_PER_DAY, Schedule, Sport, SubscriberRef, VenueId};
use crate::subscriptions::SubscriptionService;
use crate::subscriptions::contract::{
    SubscriptionCommand, SubscriptionResult, SubscriptionSummary,
};
use crate::time::fmt_hhmm;

const MAX_SUBSCRIBER_ID_CHARS: usize = 128;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SubscribeRequest {
    subscriber_id: String,
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
pub(super) struct SubscriberQuery {
    subscriber_id: String,
}

#[derive(Serialize)]
struct Subscription {
    id: i64,
    sport: &'static str,
    club: Option<String>,
    club_name: Option<String>,
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
            club: summary
                .club
                .as_ref()
                .map(|club| club.venue_id.as_str().to_owned()),
            club_name: summary.club.map(|club| club.name),
            weekday,
            date,
            start: fmt_hhmm(summary.time_range.start_minute()),
            end: fmt_hhmm(summary.time_range.end_minute()),
            courts: summary.courts,
            filter: summary.filter.as_str(),
        }
    }
}

pub(super) async fn subscribe(
    State(service): State<Arc<SubscriptionService>>,
    request: Result<Json<SubscribeRequest>, JsonRejection>,
) -> Result<Response, InvalidRequest> {
    let Json(request) = request?;
    let schedule = match (request.weekday, request.date) {
        (Some(weekday), None) => Schedule::Weekday(weekday),
        (None, Some(date)) => Schedule::Date(date),
        _ => return Ok(respond(SubscriptionResult::InvalidSchedule)),
    };
    let (Some(start_minute), Some(end_minute)) =
        (minute_of_day(&request.start), minute_of_day(&request.end))
    else {
        return Ok(invalid_request(
            "start and end must be times of day as HH:MM".into(),
        ));
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
    Ok(handle(&service, &request.subscriber_id, command).await)
}

pub(super) async fn list(
    State(service): State<Arc<SubscriptionService>>,
    query: Result<Query<SubscriberQuery>, QueryRejection>,
) -> Result<Response, InvalidRequest> {
    let Query(query) = query?;
    Ok(handle(&service, &query.subscriber_id, SubscriptionCommand::List).await)
}

pub(super) async fn unsubscribe(
    State(service): State<Arc<SubscriptionService>>,
    id: Result<Path<i64>, PathRejection>,
    query: Result<Query<SubscriberQuery>, QueryRejection>,
) -> Result<Response, InvalidRequest> {
    let Path(id) = id?;
    let Query(query) = query?;
    Ok(handle(
        &service,
        &query.subscriber_id,
        SubscriptionCommand::Unsubscribe { id },
    )
    .await)
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
    subscriber_id: &str,
    command: SubscriptionCommand,
) -> Response {
    if subscriber_id.is_empty() || subscriber_id.chars().count() > MAX_SUBSCRIBER_ID_CHARS {
        return rejected(json!({ "error": "invalid_subscriber_id" }));
    }
    let subscriber = SubscriberRef {
        integration: INTEGRATION,
        subscriber_id: subscriber_id.to_owned(),
    };
    match service.handle(&subscriber, command).await {
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
                "open_slots": open_slots.iter().map(Slot::from).collect::<Vec<_>>(),
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
            "available": available
                .into_iter()
                .map(|club| json!({ "id": club.venue_id.as_str(), "name": club.name }))
                .collect::<Vec<_>>(),
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
        SubscriptionResult::FilterNotForSport { sport, filter } => rejected(json!({
            "error": "filter_not_for_sport",
            "sport": sport.as_str(),
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
