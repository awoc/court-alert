use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::subscriptions::contract::AvailableSlotSummary;

#[derive(Serialize)]
pub(super) struct Slot<'a> {
    club: &'a str,
    court: &'a str,
    court_id: Uuid,
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
}

impl<'a> From<&'a AvailableSlotSummary> for Slot<'a> {
    fn from(slot: &'a AvailableSlotSummary) -> Self {
        Self {
            club: &slot.club,
            court: &slot.court,
            court_id: slot.court_id,
            starts_at: slot.starts_at,
            ends_at: slot.ends_at,
        }
    }
}
