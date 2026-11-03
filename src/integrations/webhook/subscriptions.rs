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
use uuid::Uuid;

use super::INTEGRATION;
use super::response::{InvalidRequest, error, invalid_request, rejected};
use super::slot::Slot;
use crate::model::{CourtFilter, MINUTES_PER_DAY, Schedule, Sport, SubscriberRef, VenueId};
use crate::subscriptions::SubscriptionService;
use crate::subscriptions::contract::{
    CourtChoice, SelectedCourtSummary, Subscribed, SubscriptionRejection, SubscriptionRequest,
    SubscriptionSummary, UnsubscribeOutcome,
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
    courts: Option<Vec<Uuid>>,
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
    courts: Option<Vec<SelectedCourt>>,
    filter: &'static str,
}

#[derive(Serialize)]
struct SelectedCourt {
    id: Option<Uuid>,
    name: Option<String>,
}

impl From<SelectedCourtSummary> for SelectedCourt {
    fn from(court: SelectedCourtSummary) -> Self {
        match court {
            SelectedCourtSummary::Identified { id, name } => Self { id: Some(id), name },
            SelectedCourtSummary::PendingName(name) => Self {
                id: None,
                name: Some(name),
            },
        }
    }
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
            courts: summary
                .courts
                .map(|courts| courts.into_iter().map(SelectedCourt::from).collect()),
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
        _ => return Ok(rejection_response(SubscriptionRejection::InvalidSchedule)),
    };
    let (Some(start_minute), Some(end_minute)) =
        (minute_of_day(&request.start), minute_of_day(&request.end))
    else {
        return Ok(invalid_request(
            "start and end must be times of day as HH:MM".into(),
        ));
    };
    let Some(subscriber) = subscriber_ref(&request.subscriber_id) else {
        return Ok(invalid_subscriber_id());
    };
    let request = SubscriptionRequest {
        sport: request.sport,
        venue: request.club,
        schedule,
        start_minute,
        end_minute,
        courts: request.courts.map(CourtChoice::Ids),
        filter: request.filter,
    };
    Ok(respond(
        service.subscribe(&subscriber, &request).await,
        |outcome| match outcome {
            Ok(subscribed) => subscribed_response(subscribed),
            Err(rejection) => rejection_response(rejection),
        },
    ))
}

pub(super) async fn list(
    State(service): State<Arc<SubscriptionService>>,
    query: Result<Query<SubscriberQuery>, QueryRejection>,
) -> Result<Response, InvalidRequest> {
    let Query(query) = query?;
    let Some(subscriber) = subscriber_ref(&query.subscriber_id) else {
        return Ok(invalid_subscriber_id());
    };
    Ok(respond(service.list(&subscriber).await, list_response))
}

pub(super) async fn unsubscribe(
    State(service): State<Arc<SubscriptionService>>,
    id: Result<Path<i64>, PathRejection>,
    query: Result<Query<SubscriberQuery>, QueryRejection>,
) -> Result<Response, InvalidRequest> {
    let Path(id) = id?;
    let Query(query) = query?;
    let Some(subscriber) = subscriber_ref(&query.subscriber_id) else {
        return Ok(invalid_subscriber_id());
    };
    Ok(respond(
        service.unsubscribe(&subscriber, id).await,
        unsubscribe_response,
    ))
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

fn subscriber_ref(subscriber_id: &str) -> Option<SubscriberRef> {
    if subscriber_id.is_empty() || subscriber_id.chars().count() > MAX_SUBSCRIBER_ID_CHARS {
        return None;
    }
    Some(SubscriberRef {
        integration: INTEGRATION,
        subscriber_id: subscriber_id.to_owned(),
    })
}

fn invalid_subscriber_id() -> Response {
    rejected(json!({ "error": "invalid_subscriber_id" }))
}

fn respond<T>(outcome: anyhow::Result<T>, success: impl FnOnce(T) -> Response) -> Response {
    match outcome {
        Ok(outcome) => success(outcome),
        Err(e) => {
            error!(error = %format!("{e:#}"), "webhook API subscription command failed");
            internal_error()
        }
    }
}

fn subscribed_response(subscribed: Subscribed) -> Response {
    (
        StatusCode::CREATED,
        Json(json!({
            "subscription": Subscription::from(subscribed.summary),
            "open_slots": subscribed.open_slots.iter().map(Slot::from).collect::<Vec<_>>(),
        })),
    )
        .into_response()
}

fn list_response(subscriptions: Vec<SubscriptionSummary>) -> Response {
    Json(json!({
        "subscriptions": subscriptions
            .into_iter()
            .map(Subscription::from)
            .collect::<Vec<_>>(),
    }))
    .into_response()
}

fn unsubscribe_response(outcome: UnsubscribeOutcome) -> Response {
    match outcome {
        UnsubscribeOutcome::Removed { .. } => StatusCode::NO_CONTENT.into_response(),
        UnsubscribeOutcome::NotFound { .. } => error(StatusCode::NOT_FOUND, "not_found"),
    }
}

fn rejection_response(rejection: SubscriptionRejection) -> Response {
    match rejection {
        SubscriptionRejection::InvalidTimeRange => {
            rejected(json!({ "error": "invalid_time_range" }))
        }
        SubscriptionRejection::InvalidSchedule => rejected(json!({ "error": "invalid_schedule" })),
        SubscriptionRejection::TooManyCourts { max } => {
            rejected(json!({ "error": "too_many_courts", "max": max }))
        }
        SubscriptionRejection::CourtNameTooLong { max } => {
            rejected(json!({ "error": "court_name_too_long", "max": max }))
        }
        SubscriptionRejection::UnknownCourts { unknown, available } => rejected(json!({
            "error": "unknown_courts",
            "unknown": match unknown {
                CourtChoice::Names(names) => json!(names),
                CourtChoice::Ids(ids) => json!(ids),
            },
            "available": available
                .iter()
                .map(|court| json!({ "id": court.id(), "name": court.name() }))
                .collect::<Vec<_>>(),
        })),
        SubscriptionRejection::CourtsNeedClub => rejected(json!({ "error": "courts_need_club" })),
        SubscriptionRejection::CourtCatalogsNotReady => {
            rejected(json!({ "error": "court_catalogs_not_ready" }))
        }
        SubscriptionRejection::UnknownClub { unknown, available } => rejected(json!({
            "error": "unknown_club",
            "unknown": unknown.as_str(),
            "available": available
                .into_iter()
                .map(|club| json!({ "id": club.venue_id.as_str(), "name": club.name }))
                .collect::<Vec<_>>(),
        })),
        SubscriptionRejection::NoClubsConfigured { sport } => rejected(json!({
            "error": "no_clubs_configured",
            "sport": sport.as_str(),
        })),
        SubscriptionRejection::FilterExcludesCourts { courts, filter } => rejected(json!({
            "error": "filter_excludes_courts",
            "courts": courts,
            "filter": filter.as_str(),
        })),
        SubscriptionRejection::FilterNotForSport { sport, filter } => rejected(json!({
            "error": "filter_not_for_sport",
            "sport": sport.as_str(),
            "filter": filter.as_str(),
        })),
    }
}

fn internal_error() -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal")
}
