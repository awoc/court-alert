use std::collections::HashMap;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::VenueId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotObservation {
    pub venue_id: VenueId,
    pub court_id: Uuid,
    pub court_name: String,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub booking_closes_at: Option<DateTime<Utc>>,
    pub available_places: u32,
    pub already_booked: bool,
    pub already_in_cart: bool,
    pub already_on_waiting_list: bool,
    pub blocked_by_resource: bool,
}

impl SlotObservation {
    pub fn into_bookable(self, now: DateTime<Utc>) -> Option<BookableSlot> {
        let booking_open = self.booking_closes_at.is_none_or(|end| end > now);
        (booking_open
            && self.available_places > 0
            && !self.already_booked
            && !self.already_in_cart
            && !self.already_on_waiting_list
            && !self.blocked_by_resource)
            .then_some(BookableSlot {
                venue_id: self.venue_id,
                court_id: self.court_id,
                court_name: self.court_name,
                starts_at: self.starts_at,
                ends_at: self.ends_at,
                available_places: self.available_places,
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookableSlot {
    pub venue_id: VenueId,
    pub court_id: Uuid,
    pub court_name: String,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub available_places: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BookableSlotId {
    pub court_id: Uuid,
    pub starts_at: DateTime<Utc>,
}

impl From<&BookableSlot> for BookableSlotId {
    fn from(slot: &BookableSlot) -> Self {
        Self {
            court_id: slot.court_id,
            starts_at: slot.starts_at,
        }
    }
}

pub type BookableSlotSnapshot = HashMap<BookableSlotId, BookableSlot>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AvailabilityChange {
    BecameBookable(BookableSlot),
    BecameUnbookable(BookableSlot),
}

impl AvailabilityChange {
    pub fn slot(&self) -> &BookableSlot {
        match self {
            Self::BecameBookable(slot) | Self::BecameUnbookable(slot) => slot,
        }
    }

    pub fn taken_ids(changes: &[Self]) -> Vec<BookableSlotId> {
        changes
            .iter()
            .filter_map(|change| match change {
                Self::BecameUnbookable(slot) => Some(BookableSlotId::from(slot)),
                Self::BecameBookable(_) => None,
            })
            .collect()
    }
}

pub fn diff_availability(
    previous: &BookableSlotSnapshot,
    current: &BookableSlotSnapshot,
) -> Vec<AvailabilityChange> {
    let mut changes = Vec::new();
    for (id, slot) in current {
        if !previous.contains_key(id) {
            changes.push(AvailabilityChange::BecameBookable(slot.clone()));
        }
    }
    for (id, slot) in previous {
        if !current.contains_key(id) {
            changes.push(AvailabilityChange::BecameUnbookable(slot.clone()));
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn observation() -> SlotObservation {
        let starts_at = Utc.with_ymd_and_hms(2026, 6, 2, 8, 0, 0).unwrap();
        SlotObservation {
            venue_id: VenueId::new("zhs-munich"),
            court_id: Uuid::nil(),
            court_name: "Court 1".into(),
            starts_at,
            ends_at: starts_at + Duration::hours(1),
            booking_closes_at: None,
            available_places: 1,
            already_booked: false,
            already_in_cart: false,
            already_on_waiting_list: false,
            blocked_by_resource: false,
        }
    }

    #[test]
    fn open_observation_becomes_bookable() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        assert!(observation().into_bookable(now).is_some());
    }

    #[test]
    fn closed_booking_window_is_not_bookable() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let mut observed = observation();
        observed.booking_closes_at = Some(now);
        assert!(observed.into_bookable(now).is_none());
    }

    #[test]
    fn provider_restrictions_prevent_bookability() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let mut observed = observation();
        observed.already_booked = true;
        assert!(observed.into_bookable(now).is_none());
    }

    #[test]
    fn diff_reports_additions_and_removals() {
        let now = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let first = observation().into_bookable(now).unwrap();
        let mut second = first.clone();
        second.starts_at += Duration::hours(1);
        second.ends_at += Duration::hours(1);

        let previous = [(BookableSlotId::from(&first), first)]
            .into_iter()
            .collect();
        let current = [(BookableSlotId::from(&second), second)]
            .into_iter()
            .collect();
        let changes = diff_availability(&previous, &current);

        assert_eq!(changes.len(), 2);
        assert!(
            changes
                .iter()
                .any(|c| matches!(c, AvailabilityChange::BecameBookable(_)))
        );
        assert!(
            changes
                .iter()
                .any(|c| matches!(c, AvailabilityChange::BecameUnbookable(_)))
        );
    }
}
