pub mod contract;

mod commands;
mod dispatch;
mod matcher;
#[cfg(test)]
mod testing;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use chrono::{DateTime, NaiveDate, Utc};

use crate::model::{CourtCatalog, ProviderUserRef, SurfaceFilter};
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
    courts: Arc<CourtCatalog>,
    default_surface: SurfaceFilter,
    clock: Arc<dyn Clock>,
}

impl SubscriptionService {
    pub fn new(
        store: Arc<dyn SubscriptionRepository>,
        slot_snapshots: Arc<dyn BookableSlotSnapshotRepository>,
        admins: HashSet<ProviderUserRef>,
        courts: Arc<CourtCatalog>,
        default_surface: SurfaceFilter,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            slot_snapshots,
            senders: RwLock::new(HashMap::new()),
            admins,
            courts,
            default_surface,
            clock,
        }
    }

    fn is_admin(&self, user: &ProviderUserRef) -> bool {
        self.admins.contains(user)
    }

    pub fn register_sender(&self, provider: &str, sender: Arc<dyn DirectMessageSender>) {
        self.senders
            .write()
            .expect("senders lock poisoned")
            .insert(provider.to_string(), sender);
    }
}
