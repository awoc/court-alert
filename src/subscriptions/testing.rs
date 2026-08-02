use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, NaiveDate, Utc, Weekday};

use crate::model::{
    BookableSlot, BookableSlotSnapshot, Court, CourtAttributes, CourtCatalog, CourtFilter,
    CourtSurface, ProviderUserRef, Schedule, Sport, VenueId, VenueRegistry,
};
use crate::ports::BookableSlotSnapshotRepository;
use crate::store::SqliteStore;
use crate::subscriptions::contract::SubscriptionCommand;
use uuid::Uuid;

use super::{BerlinClock, Clock, SubscriptionService};

pub(super) fn uref(id: &str) -> ProviderUserRef {
    ProviderUserRef {
        provider: "discord".into(),
        user_id: id.into(),
    }
}

pub(super) const SYNTHETIC_COURT: &str = "Court 19 - Synthetic";

pub(super) fn venue_id() -> VenueId {
    VenueId::new("zhs-munich")
}

pub(super) fn catalog() -> CourtCatalog {
    CourtCatalog::new(vec![
        Court::new(
            Uuid::from_u128(2),
            "Court 2".into(),
            CourtAttributes::tennis(CourtSurface::Clay),
        ),
        Court::new(
            Uuid::from_u128(5),
            "Court 5".into(),
            CourtAttributes::tennis(CourtSurface::Clay),
        ),
        Court::new(
            Uuid::from_u128(19),
            SYNTHETIC_COURT.into(),
            CourtAttributes::tennis(CourtSurface::Synthetic),
        ),
    ])
}

pub(super) fn registry() -> Arc<RwLock<VenueRegistry>> {
    let mut registry = VenueRegistry::new();
    registry.register(venue_id(), Sport::Tennis);
    registry.set_catalog(venue_id(), catalog());
    Arc::new(RwLock::new(registry))
}

pub(super) fn court_id(name: &str) -> Uuid {
    catalog()
        .find_by_name(name)
        .map_or_else(Uuid::new_v4, |court| court.id())
}

pub(super) struct FixedClock(pub(super) DateTime<Utc>);

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

fn noon_utc(date: NaiveDate) -> DateTime<Utc> {
    date.and_hms_opt(12, 0, 0).expect("valid time").and_utc()
}

async fn store() -> Arc<SqliteStore> {
    Arc::new(SqliteStore::open_in_memory().await.unwrap())
}

fn build(
    store: Arc<SqliteStore>,
    admins: HashSet<ProviderUserRef>,
    clock: Arc<dyn Clock>,
) -> Arc<SubscriptionService> {
    Arc::new(SubscriptionService::new(
        store.clone(),
        store,
        admins,
        registry(),
        CourtFilter::Any,
        clock,
    ))
}

pub(super) async fn service() -> Arc<SubscriptionService> {
    build(store().await, HashSet::new(), Arc::new(BerlinClock))
}

pub(super) async fn service_defaulting_to_clay() -> Arc<SubscriptionService> {
    let store = store().await;
    Arc::new(SubscriptionService::new(
        store.clone(),
        store,
        HashSet::new(),
        registry(),
        CourtFilter::CLAY,
        Arc::new(BerlinClock),
    ))
}

pub(super) async fn service_with_clock(today: NaiveDate) -> Arc<SubscriptionService> {
    build(
        store().await,
        HashSet::new(),
        Arc::new(FixedClock(noon_utc(today))),
    )
}

pub(super) async fn service_with_store() -> (Arc<SubscriptionService>, Arc<SqliteStore>) {
    let store = store().await;
    (
        build(store.clone(), HashSet::new(), Arc::new(BerlinClock)),
        store,
    )
}

pub(super) fn admin_uref() -> ProviderUserRef {
    uref("999")
}

pub(super) async fn service_with_admin() -> Arc<SubscriptionService> {
    let mut admins = HashSet::new();
    admins.insert(admin_uref());
    build(store().await, admins, Arc::new(BerlinClock))
}

pub(super) fn subscribe_cmd(from: u32, to: u32) -> SubscriptionCommand {
    SubscriptionCommand::Subscribe {
        schedule: Schedule::Weekday(Weekday::Tue),
        start_minute: from,
        end_minute: to,
        courts: None,
        filter: None,
    }
}

pub(super) fn date_subscribe_cmd(date: NaiveDate, from: u32, to: u32) -> SubscriptionCommand {
    SubscriptionCommand::Subscribe {
        schedule: Schedule::Date(date),
        start_minute: from,
        end_minute: to,
        courts: None,
        filter: None,
    }
}

pub(super) fn open_slot(court: &str, starts_at: DateTime<Utc>) -> BookableSlot {
    BookableSlot {
        venue_id: venue_id(),
        court_id: court_id(court),
        court_name: court.into(),
        starts_at,
        ends_at: starts_at + chrono::Duration::hours(1),
        available_places: 1,
    }
}

pub(super) async fn service_with_slots(
    now: DateTime<Utc>,
    slots: Vec<BookableSlot>,
) -> Arc<SubscriptionService> {
    let store = store().await;
    store.replace_snapshot(slots).await.unwrap();
    Arc::new(SubscriptionService::new(
        store.clone(),
        store,
        HashSet::new(),
        registry(),
        CourtFilter::Any,
        Arc::new(FixedClock(now)),
    ))
}

struct FailingSlotSnapshotRepository;

#[async_trait::async_trait]
impl BookableSlotSnapshotRepository for FailingSlotSnapshotRepository {
    async fn load_snapshot(&self) -> anyhow::Result<BookableSlotSnapshot> {
        anyhow::bail!("simulated slot-snapshot failure")
    }

    async fn replace_snapshot(&self, _slots: Vec<BookableSlot>) -> anyhow::Result<()> {
        anyhow::bail!("simulated slot-snapshot failure")
    }
}

pub(super) async fn service_with_failing_slot_snapshot() -> Arc<SubscriptionService> {
    Arc::new(SubscriptionService::new(
        store().await,
        Arc::new(FailingSlotSnapshotRepository),
        HashSet::new(),
        registry(),
        CourtFilter::Any,
        Arc::new(BerlinClock),
    ))
}
