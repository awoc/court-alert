use chrono::{DateTime, Utc};

use crate::model::{CourtFilter, ProviderUserRef, Schedule, Sport, TimeRange};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionSummary {
    pub id: i64,
    pub sport: Sport,
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
    pub club: String,
    pub court: String,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
}
