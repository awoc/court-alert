use chrono::{DateTime, Utc};

use crate::model::{BookableSlot, CourtFilter, ProviderUserRef, Schedule, Subscription, TimeRange};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionSummary {
    pub id: i64,
    pub schedule: Schedule,
    pub time_range: TimeRange,
    pub courts: Option<Vec<String>>,
    pub filter: CourtFilter,
}

impl From<Subscription> for SubscriptionSummary {
    fn from(subscription: Subscription) -> Self {
        Self {
            id: subscription.id,
            schedule: subscription.schedule,
            time_range: subscription.time_range,
            courts: subscription.courts,
            filter: subscription.filter,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedSubscriptionSummary {
    pub user: ProviderUserRef,
    pub summary: SubscriptionSummary,
}

impl From<Subscription> for OwnedSubscriptionSummary {
    fn from(subscription: Subscription) -> Self {
        let Subscription {
            id,
            user,
            schedule,
            time_range,
            courts,
            filter,
        } = subscription;
        Self {
            user,
            summary: SubscriptionSummary {
                id,
                schedule,
                time_range,
                courts,
                filter,
            },
        }
    }
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
