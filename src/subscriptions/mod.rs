pub mod contract;

mod commands;
mod dispatch;
mod matcher;
#[cfg(test)]
pub(crate) mod testing;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use chrono::{DateTime, NaiveDate, Utc};

use crate::model::{
    CourtCatalog, CourtFilter, IntegrationId, Sport, SubscriberRef, VenueId, VenueRegistry,
};
use crate::ports::{BookableSlotSnapshotRepository, SubscriptionRepository};
use contract::AlertSender;

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
    senders: RwLock<HashMap<IntegrationId, Arc<dyn AlertSender>>>,
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
            senders: RwLock::new(HashMap::new()),
            admins,
            registry,
            tennis_default_filter,
            clock,
        }
    }

    fn is_admin(&self, subscriber: &SubscriberRef) -> bool {
        self.admins.contains(subscriber)
    }

    fn catalogs_for(
        &self,
        sport: Sport,
        venue: Option<&VenueId>,
    ) -> Vec<(VenueId, Arc<CourtCatalog>)> {
        let registry = self.registry.read().expect("venue registry poisoned");
        match venue {
            Some(venue_id) => registry
                .catalog(venue_id)
                .map(|catalog| vec![(venue_id.clone(), catalog)])
                .unwrap_or_default(),
            None => registry.catalogs_for_sport(sport),
        }
    }

    fn court_names(&self, sport: Sport, venue: Option<&VenueId>) -> Vec<String> {
        self.catalogs_for(sport, venue)
            .iter()
            .flat_map(|(_, catalog)| catalog.names())
            .collect()
    }

    pub fn clubs_of(&self, sport: Sport) -> Vec<contract::ClubRef> {
        let registry = self.registry.read().expect("venue registry poisoned");
        registry
            .venues_of_sport(sport)
            .into_iter()
            .map(|venue_id| contract::ClubRef {
                name: registry.club_label(&venue_id),
                venue_id,
            })
            .collect()
    }

    pub fn club_directory(&self, sport: Sport) -> Vec<contract::ClubSummary> {
        let registry = self.registry.read().expect("venue registry poisoned");
        registry
            .venues_of_sport(sport)
            .into_iter()
            .map(|venue_id| {
                let courts = registry
                    .catalog(&venue_id)
                    .map(|catalog| {
                        catalog
                            .courts()
                            .iter()
                            .map(contract::CourtSummary::from)
                            .collect()
                    })
                    .unwrap_or_default();
                contract::ClubSummary {
                    name: registry.club_label(&venue_id),
                    venue_id,
                    sport,
                    courts,
                }
            })
            .collect()
    }

    pub fn register_sender(&self, integration: IntegrationId, sender: Arc<dyn AlertSender>) {
        self.senders
            .write()
            .expect("senders lock poisoned")
            .insert(integration, sender);
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
