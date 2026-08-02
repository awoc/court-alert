pub mod contract;

mod commands;
mod dispatch;
mod matcher;
#[cfg(test)]
mod testing;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use chrono::{DateTime, NaiveDate, Utc};

use crate::model::{CourtCatalog, CourtFilter, ProviderUserRef, Sport, VenueId, VenueRegistry};
use crate::ports::{BookableSlotSnapshotRepository, SubscriptionRepository};
use contract::DirectMessageSender;

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
    senders: RwLock<HashMap<String, Arc<dyn DirectMessageSender>>>,
    admins: HashSet<ProviderUserRef>,
    registry: Arc<RwLock<VenueRegistry>>,
    tennis_default_filter: CourtFilter,
    clock: Arc<dyn Clock>,
}

impl SubscriptionService {
    pub fn new(
        store: Arc<dyn SubscriptionRepository>,
        slot_snapshots: Arc<dyn BookableSlotSnapshotRepository>,
        admins: HashSet<ProviderUserRef>,
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

    fn is_admin(&self, user: &ProviderUserRef) -> bool {
        self.admins.contains(user)
    }

    /// The court catalogs a command operates over, narrowed to one club when
    /// the subscription names one.
    ///
    /// Cloned out of the registry so the guard is released before the caller
    /// does anything slow.
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

    /// The clubs a sport's command offers, as (id, display name) pairs.
    pub fn clubs_of(&self, sport: Sport) -> Vec<(VenueId, String)> {
        let registry = self.registry.read().expect("venue registry poisoned");
        registry
            .venues_of_sport(sport)
            .into_iter()
            .map(|venue_id| {
                let name = registry
                    .display_name(&venue_id)
                    .unwrap_or_else(|| venue_id.as_str())
                    .to_owned();
                (venue_id, name)
            })
            .collect()
    }

    pub fn register_sender(&self, provider: &str, sender: Arc<dyn DirectMessageSender>) {
        self.senders
            .write()
            .expect("senders lock poisoned")
            .insert(provider.to_string(), sender);
    }
}
