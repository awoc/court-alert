use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::model::{
    Court, CourtFilter, CourtLocation, CourtSurface, Schedule, Sport, SubscriberRef, TimeRange,
    VenueId,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionSummary {
    pub id: i64,
    pub sport: Sport,
    pub club: Option<ClubRef>,
    pub schedule: Schedule,
    pub time_range: TimeRange,
    pub courts: Option<Vec<SelectedCourtSummary>>,
    pub filter: CourtFilter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectedCourtSummary {
    /// `name` comes from the current catalog: `None` while the catalog is not
    /// ready or once the court has left it.
    Identified {
        id: Uuid,
        name: Option<String>,
    },
    PendingName(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedSubscriptionSummary {
    pub subscriber: SubscriberRef,
    pub summary: SubscriptionSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableSlotSummary {
    pub club: String,
    pub court: String,
    pub court_id: Uuid,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClubRef {
    pub venue_id: VenueId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClubSummary {
    pub venue_id: VenueId,
    pub name: String,
    pub sport: Sport,
    pub courts: Vec<CourtSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CourtSummary {
    pub id: Uuid,
    pub name: String,
    pub surface: Option<CourtSurface>,
    pub location: Option<CourtLocation>,
}

impl From<&Court> for CourtSummary {
    fn from(court: &Court) -> Self {
        Self {
            id: court.id(),
            name: court.name().to_owned(),
            surface: court.attributes().surface(),
            location: court.attributes().location(),
        }
    }
}
