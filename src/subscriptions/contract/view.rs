use chrono::{DateTime, Utc};

use crate::model::{BookableSlot, CourtFilter, ProviderUserRef, Schedule, Sport, TimeRange};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionSummary {
    pub id: i64,
    pub sport: Sport,
    /// The club's display name, or `None` for every club of the sport.
    /// Resolved by the service, so renderers never handle venue ids.
    pub club: Option<String>,
    pub schedule: Schedule,
    pub time_range: TimeRange,
    pub courts: Option<Vec<String>>,
    pub filter: CourtFilter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedSubscriptionSummary {
    pub user: ProviderUserRef,
    pub summary: SubscriptionSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableSlotSummary {
    pub court: String,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
}

impl From<&BookableSlot> for AvailableSlotSummary {
    fn from(slot: &BookableSlot) -> Self {
        Self {
            court: slot.court_name.clone(),
            starts_at: slot.starts_at,
            ends_at: slot.ends_at,
        }
    }
}
