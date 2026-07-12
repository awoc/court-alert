use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Utc, Weekday};

use crate::domain::{BookableSlot, BookableSlotSnapshot, ProviderUserRef, Schedule};
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

fn known_courts() -> Vec<String> {
    vec!["Court 2".into(), "Court 5".into()]
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
        known_courts(),
        clock,
    ))
}

pub(super) async fn service() -> Arc<SubscriptionService> {
    build(store().await, HashSet::new(), Arc::new(BerlinClock))
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
    }
}

pub(super) fn date_subscribe_cmd(date: NaiveDate, from: u32, to: u32) -> SubscriptionCommand {
    SubscriptionCommand::Subscribe {
        schedule: Schedule::Date(date),
        start_minute: from,
        end_minute: to,
        courts: None,
    }
}

pub(super) fn open_slot(court: &str, starts_at: DateTime<Utc>) -> BookableSlot {
    BookableSlot {
        court_id: Uuid::new_v4(),
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
        known_courts(),
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
        known_courts(),
        Arc::new(BerlinClock),
    ))
}
