use crate::model::Schedule;

use super::{AvailableSlotSummary, OwnedSubscriptionSummary, SubscriptionSummary};

#[derive(Debug, Clone)]
pub enum SubscriptionCommand {
    Subscribe {
        schedule: Schedule,
        start_minute: u32,
        end_minute: u32,
        courts: Option<Vec<String>>,
    },
    List,
    Unsubscribe {
        id: i64,
    },
    ListAll,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriptionResult {
    Subscribed {
        summary: SubscriptionSummary,
        open_slots: Vec<AvailableSlotSummary>,
    },
    SubscriptionList(Vec<SubscriptionSummary>),
    Unsubscribed {
        id: i64,
    },
    NotFound {
        id: i64,
    },
    InvalidTimeRange,
    InvalidSchedule,
    UnknownCourts {
        unknown: Vec<String>,
        available: Vec<String>,
    },
    AllSubscriptions(Vec<OwnedSubscriptionSummary>),
    NotAuthorized,
}
