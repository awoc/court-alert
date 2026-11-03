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
use super::response::ApiError;
use super::slot::Slot;
use crate::model::{CourtFilter, MINUTES_PER_DAY, Schedule, Sport, SubscriberRef, VenueId};
use crate::subscriptions::SubscriptionService;
use crate::subscriptions::contract::{
    CourtChoice, SelectedCourtSummary, Subscribed, SubscriptionRequest, SubscriptionSummary,
    UnsubscribeOutcome,
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
) -> Result<Response, ApiError> {
    let Json(request) = request?;
    let schedule = match (request.weekday, request.date) {
        (Some(weekday), None) => Schedule::Weekday(weekday),
        (None, Some(date)) => Schedule::Date(date),
        _ => return Err(ApiError::InvalidSchedule),
    };
    let (Some(start_minute), Some(end_minute)) =
        (minute_of_day(&request.start), minute_of_day(&request.end))
    else {
        return Err(ApiError::invalid_request(
            "start and end must be times of day as HH:MM",
        ));
    };
    let subscriber = subscriber_ref(&request.subscriber_id).ok_or(ApiError::InvalidSubscriberId)?;
    let request = SubscriptionRequest {
        sport: request.sport,
        venue: request.club,
        schedule,
        start_minute,
        end_minute,
        courts: request.courts.map(CourtChoice::Ids),
        filter: request.filter,
    };
    let subscribed = service
        .subscribe(&subscriber, &request)
        .await
        .map_err(internal_error)??;
    Ok(subscribed_response(subscribed))
}

pub(super) async fn list(
    State(service): State<Arc<SubscriptionService>>,
    query: Result<Query<SubscriberQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let Query(query) = query?;
    let subscriber = subscriber_ref(&query.subscriber_id).ok_or(ApiError::InvalidSubscriberId)?;
    let subscriptions = service.list(&subscriber).await.map_err(internal_error)?;
    Ok(list_response(subscriptions))
}

pub(super) async fn unsubscribe(
    State(service): State<Arc<SubscriptionService>>,
    id: Result<Path<i64>, PathRejection>,
    query: Result<Query<SubscriberQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let Path(id) = id?;
    let Query(query) = query?;
    let subscriber = subscriber_ref(&query.subscriber_id).ok_or(ApiError::InvalidSubscriberId)?;
    match service
        .unsubscribe(&subscriber, id)
        .await
        .map_err(internal_error)?
    {
        UnsubscribeOutcome::Removed { .. } => Ok(StatusCode::NO_CONTENT.into_response()),
        UnsubscribeOutcome::NotFound { .. } => Err(ApiError::NotFound),
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

fn subscriber_ref(subscriber_id: &str) -> Option<SubscriberRef> {
    if subscriber_id.is_empty() || subscriber_id.chars().count() > MAX_SUBSCRIBER_ID_CHARS {
        return None;
    }
    Some(SubscriberRef {
        integration: INTEGRATION,
        subscriber_id: subscriber_id.to_owned(),
    })
}

fn internal_error(e: anyhow::Error) -> ApiError {
    error!(error = %format!("{e:#}"), "webhook API subscription command failed");
    ApiError::Internal
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
