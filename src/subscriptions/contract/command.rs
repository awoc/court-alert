pub use crate::model::{CourtChoice, SubscriptionRejection, SubscriptionRequest};

use super::{AvailableSlotSummary, SubscriptionSummary};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscribed {
    pub summary: SubscriptionSummary,
    pub open_slots: Vec<AvailableSlotSummary>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsubscribeOutcome {
    Removed { id: i64 },
    NotFound { id: i64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotAuthorized;
