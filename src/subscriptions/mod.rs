pub mod contract;

mod club_directory;
mod commands;
mod court_conversion;
mod dispatch;
mod matcher;
#[cfg(test)]
pub(crate) mod testing;

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, NaiveDate, Utc};

use crate::model::{CourtFilter, SubscriberRef, VenueRegistry};
use crate::ports::{BookableSlotSnapshotRepository, SubscriptionRepository};

pub use club_directory::ClubDirectory;
pub use court_conversion::{ConversionPass, CourtNameConversion};
pub use dispatch::AlertDispatcher;

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;

    fn today(&self) -> NaiveDate {
        crate::time::berlin_date(self.now())
    }
}

pub struct BerlinClock;

impl Clock for BerlinClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

pub struct SubscriptionService {
    store: Arc<dyn SubscriptionRepository>,
    slot_snapshots: Arc<dyn BookableSlotSnapshotRepository>,
    admins: HashSet<SubscriberRef>,
    registry: Arc<RwLock<VenueRegistry>>,
    tennis_default_filter: CourtFilter,
    clock: Arc<dyn Clock>,
}

impl SubscriptionService {
    pub fn new(
        store: Arc<dyn SubscriptionRepository>,
        slot_snapshots: Arc<dyn BookableSlotSnapshotRepository>,
        admins: HashSet<SubscriberRef>,
        registry: Arc<RwLock<VenueRegistry>>,
        tennis_default_filter: CourtFilter,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            slot_snapshots,
            admins,
            registry,
            tennis_default_filter,
            clock,
        }
    }

    fn is_admin(&self, subscriber: &SubscriberRef) -> bool {
        self.admins.contains(subscriber)
    }
}

fn slot_summary(
    registry: &VenueRegistry,
    slot: &crate::model::BookableSlot,
) -> contract::AvailableSlotSummary {
    contract::AvailableSlotSummary {
        club: registry.club_label(&slot.venue_id),
        court: slot.court_name.clone(),
        court_id: slot.court_id,
        starts_at: slot.starts_at,
        ends_at: slot.ends_at,
    }
}
