use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::response::ApiError;
use crate::model::Sport;
use crate::subscriptions::ClubDirectory;
use crate::subscriptions::contract::{ClubSummary, CourtSummary};

#[derive(Deserialize)]
pub(super) struct ClubsQuery {
    sport: Sport,
}

#[derive(Serialize)]
pub(super) struct ClubList {
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

pub(super) async fn list(
    State(directory): State<ClubDirectory>,
    query: Result<Query<ClubsQuery>, QueryRejection>,
) -> Result<Json<ClubList>, ApiError> {
    let Query(query) = query?;
    let clubs = directory
        .summaries_of(query.sport)
        .into_iter()
        .map(Club::from)
        .collect();
    Ok(Json(ClubList { clubs }))
}
