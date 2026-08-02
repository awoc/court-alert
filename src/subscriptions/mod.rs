pub mod contract;

mod commands;
mod dispatch;
mod matcher;
#[cfg(test)]
mod testing;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use chrono::{DateTime, NaiveDate, Utc};

use crate::model::{CourtFilter, ProviderUserRef, Sport, VenueRegistry};
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
    default_surface: CourtFilter,
    clock: Arc<dyn Clock>,
}

impl SubscriptionService {
    pub fn new(
        store: Arc<dyn SubscriptionRepository>,
        slot_snapshots: Arc<dyn BookableSlotSnapshotRepository>,
        admins: HashSet<ProviderUserRef>,
        registry: Arc<RwLock<VenueRegistry>>,
        default_surface: CourtFilter,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            slot_snapshots,
            senders: RwLock::new(HashMap::new()),
            admins,
            registry,
            default_surface,
            clock,
        }
    }

    fn is_admin(&self, user: &ProviderUserRef) -> bool {
        self.admins.contains(user)
    }

    /// The court catalogs `/subscribe` operates over.
    ///
    /// Cloned out of the registry so the guard is released before the caller
    /// does anything slow. With a single tennis venue this is exactly today's
    /// catalog; the per-subscription venue scoping arrives with `/padel`.
    fn tennis_catalogs(&self) -> Vec<(crate::model::VenueId, Arc<crate::model::CourtCatalog>)> {
        self.registry
            .read()
            .expect("venue registry poisoned")
            .catalogs_for_sport(Sport::Tennis)
    }

    fn court_names(&self) -> Vec<String> {
        self.tennis_catalogs()
            .iter()
            .flat_map(|(_, catalog)| catalog.names())
            .collect()
    }

    pub fn register_sender(&self, provider: &str, sender: Arc<dyn DirectMessageSender>) {
        self.senders
            .write()
            .expect("senders lock poisoned")
            .insert(provider.to_string(), sender);
    }
}
