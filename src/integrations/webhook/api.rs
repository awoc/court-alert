use std::sync::Arc;

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::routing::{delete, get};
use axum::{Json, Router, middleware};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::auth::require_signature;
use super::response::InvalidRequest;
use super::subscriptions;
use crate::model::Sport;
use crate::subscriptions::contract::{ClubSummary, CourtSummary};
use crate::subscriptions::{ClubDirectory, SubscriptionService};

pub(super) fn router(
    service: Arc<SubscriptionService>,
    directory: ClubDirectory,
    api_secret: Arc<str>,
) -> Router {
    Router::new()
        .route("/clubs", get(clubs).with_state(directory))
        .route(
            "/subscriptions",
            get(subscriptions::list).post(subscriptions::subscribe),
        )
        .route("/subscriptions/{id}", delete(subscriptions::unsubscribe))
        .with_state(service)
        .layer(middleware::from_fn_with_state(
            api_secret,
            require_signature,
        ))
}

#[derive(Deserialize)]
struct ClubsQuery {
    sport: Sport,
}

#[derive(Serialize)]
struct ClubList {
    clubs: Vec<Club>,
}

#[derive(Serialize)]
struct Club {
    id: String,
    name: String,
    sport: &'static str,
    courts: Vec<Court>,
}

#[derive(Serialize)]
struct Court {
    id: Uuid,
    name: String,
    surface: Option<&'static str>,
    location: Option<&'static str>,
}

impl From<ClubSummary> for Club {
    fn from(club: ClubSummary) -> Self {
        Self {
            id: club.venue_id.as_str().to_owned(),
            name: club.name,
            sport: club.sport.as_str(),
            courts: club.courts.into_iter().map(Court::from).collect(),
        }
    }
}

impl From<CourtSummary> for Court {
    fn from(court: CourtSummary) -> Self {
        Self {
            id: court.id,
            name: court.name,
            surface: court.surface.map(|surface| surface.as_str()),
            location: court.location.map(|location| location.as_str()),
        }
    }
}

async fn clubs(
    State(directory): State<ClubDirectory>,
    query: Result<Query<ClubsQuery>, QueryRejection>,
) -> Result<Json<ClubList>, InvalidRequest> {
    let Query(query) = query?;
    let clubs = directory
        .summaries_of(query.sport)
        .into_iter()
        .map(Club::from)
        .collect();
    Ok(Json(ClubList { clubs }))
}
