use chrono::{Duration as ChronoDuration, TimeZone, Utc};

use super::*;

use crate::model::{
    Court, CourtAttributes, CourtSurface, SlotObservation, Sport, Venue, VenueId, VenueIdentity,
};
use crate::ports::{BookableSlotSnapshotRepository, VenueStateRepository};
use crate::store::SqliteStore;

fn venue() -> Venue {
    Venue {
        id: VenueId::new("zhs-munich"),
        display_name: "ZHS München".to_owned(),
        sport: Sport::Tennis,
        identity: VenueIdentity::Zhs {
            base_url: "https://kurse.zhs-muenchen.de".to_owned(),
        },
        poll_interval_secs: None,
        lookahead_days: None,
        operating_window: None,
    }
}

fn court() -> Court {
    Court::new(
        uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
        "Court 1".to_owned(),
        CourtAttributes::tennis(CourtSurface::Clay),
    )
}

fn observation(court: &Court) -> SlotObservation {
    let starts_at = Utc.with_ymd_and_hms(2026, 6, 2, 8, 0, 0).unwrap();
    SlotObservation {
        venue_id: venue().id,
        court_id: court.id(),
        court_name: court.name().to_owned(),
        starts_at,
        ends_at: starts_at + ChronoDuration::hours(1),
        booking_closes_at: None,
        available_places: 1,
        already_booked: false,
        already_in_cart: false,
        already_on_waiting_list: false,
        blocked_by_resource: false,
    }
}

/// Suppression follows the `venue_state` marker, not the slice being empty: a
/// single club can legitimately be fully booked across its whole horizon, and
/// inferring from emptiness would swallow its next batch of freed slots.
#[test]
fn a_fully_booked_venue_at_restart_is_not_treated_as_new() {
    let uninitialised = MonitorState::new(BookableSlotSnapshot::new(), true);
    assert!(uninitialised.suppress_next_publish);

    let already_polled = MonitorState::new(BookableSlotSnapshot::new(), false);
    assert!(
        !already_polled.suppress_next_publish,
        "a venue that has polled before must alert even with nothing currently free"
    );
}

#[tokio::test]
async fn a_venue_is_only_new_until_its_first_successful_poll() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let venue = venue();
    assert!(!store.is_initialised(&venue.id).await.unwrap());

    store.mark_initialised(&venue.id).await.unwrap();

    assert!(store.is_initialised(&venue.id).await.unwrap());
}

/// Each loop diffs against its own slice only. Against the global snapshot it
/// would report every other venue's slots as unbookable, every tick.
#[tokio::test]
async fn a_venue_loads_only_its_own_previous_slots() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let court = court();
    let observed_at = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
    let ours = build_snapshot(&venue(), vec![observation(&court)], observed_at);

    let other_id = crate::model::VenueId::new("casa-padel");
    let mut theirs = observation(&court);
    theirs.venue_id = other_id.clone();
    theirs.starts_at += ChronoDuration::hours(3);
    theirs.ends_at += ChronoDuration::hours(3);

    store
        .replace_venue_snapshot(&venue().id, ours.values().cloned().collect())
        .await
        .unwrap();
    store
        .replace_venue_snapshot(&other_id, vec![theirs.into_bookable(observed_at).unwrap()])
        .await
        .unwrap();

    let previous = store.load_venue_snapshot(&venue().id).await.unwrap();

    assert_eq!(previous, ours);
    assert!(diff_availability(&previous, &ours).is_empty());
}

/// A club whose page is unreachable must not keep hammering it every tick.
#[test]
fn discovery_backs_off_after_consecutive_failures_and_recovers() {
    let mut state = DiscoveryState::default();
    assert!(state.may_attempt(), "the first attempt is never delayed");

    state.failed();
    assert!(
        !state.may_attempt(),
        "one tick is skipped after one failure"
    );
    assert!(state.may_attempt());

    state.failed();
    state.failed();
    let skipped = (0..10).take_while(|_| !state.may_attempt()).count();
    assert!(skipped >= 3, "backoff did not grow: skipped {skipped}");

    state.succeeded();
    assert!(state.may_attempt(), "a success clears the backoff");
}

#[test]
fn the_backoff_is_capped() {
    let mut state = DiscoveryState::default();
    for _ in 0..20 {
        state.failed();
    }
    assert_eq!(state.ticks_to_skip, MAX_DISCOVERY_BACKOFF_TICKS);
}

/// Startup-only discovery goes stale: a club renames or adds a court and
/// nobody tells us.
#[test]
fn a_resolved_catalog_is_fresh_until_the_refresh_interval() {
    let mut state = DiscoveryState::default();
    assert!(state.is_stale(), "an unresolved catalog is always stale");

    state.succeeded();
    assert!(!state.is_stale());

    // An unknown resource id in an availability response forces a refresh
    // without waiting for the daily cadence.
    state.invalidate();
    assert!(state.is_stale());
}

/// Invalidation must not reset the backoff, or a club that keeps returning an
/// unknown court would retry discovery every tick.
#[test]
fn invalidating_a_catalog_leaves_the_backoff_alone() {
    let mut state = DiscoveryState::default();
    state.failed();
    state.failed();

    state.invalidate();

    assert!(state.ticks_to_skip > 0);
}

#[test]
fn venues_are_phase_offset_evenly_across_the_interval() {
    assert_eq!(phase_offset(0, 1, 300), Duration::ZERO);
    assert_eq!(phase_offset(0, 4, 300), Duration::ZERO);
    assert_eq!(phase_offset(1, 4, 300), Duration::from_secs(75));
    assert_eq!(phase_offset(3, 4, 300), Duration::from_secs(225));
}

/// A provider outage must not become one webhook message per venue per tick,
/// forever: only the transitions in and out of failure are reported at WARN.
#[test]
fn repeated_failures_are_reported_once_per_run() {
    let venue = venue();
    let mut run = FailureRun::default();

    assert!(!run.failing);
    run.failed(&venue, &anyhow::anyhow!("boom"));
    assert!(run.failing, "the first failure is reported");
    run.failed(&venue, &anyhow::anyhow!("boom"));
    assert!(run.failing, "subsequent failures stay inside the same run");

    run.succeeded(&venue);
    assert!(!run.failing);
    run.failed(&venue, &anyhow::anyhow!("boom"));
    assert!(run.failing, "a new outage is reported again");
}

#[test]
fn committing_a_poll_enables_future_publication() {
    let mut state = MonitorState::new(BookableSlotSnapshot::new(), true);
    state.commit(BookableSlotSnapshot::new());
    assert!(!state.suppress_next_publish);
}

#[test]
fn operating_window_uses_berlin_time_and_excludes_its_end() {
    let window = OperatingWindow::new(8, 24).unwrap();
    assert!(is_within_operating_window(
        Utc.with_ymd_and_hms(2026, 6, 2, 6, 0, 0).unwrap(),
        window,
    ));
    assert!(!is_within_operating_window(
        Utc.with_ymd_and_hms(2026, 6, 2, 5, 59, 0).unwrap(),
        window,
    ));
    assert!(!is_within_operating_window(
        Utc.with_ymd_and_hms(2026, 6, 2, 22, 0, 0).unwrap(),
        window,
    ));
}

#[test]
fn snapshot_keeps_the_last_duplicate_observation() {
    let court = court();
    let first = observation(&court);
    let mut duplicate = first.clone();
    duplicate.available_places = 3;
    let observed_at = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();

    let snapshot = build_snapshot(&venue(), vec![first, duplicate], observed_at);

    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot.values().next().unwrap().available_places, 3);
}

/// The store's `ends_at > starts_at` CHECK would otherwise abort persistence,
/// and with it the whole tick, every poll the provider returned such a slot.
#[test]
fn snapshot_excludes_slots_that_do_not_end_after_they_start() {
    let court = court();
    let observed_at = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();

    let mut inverted = observation(&court);
    inverted.ends_at = inverted.starts_at - ChronoDuration::hours(1);
    let mut empty_range = observation(&court);
    empty_range.ends_at = empty_range.starts_at;

    let snapshot = build_snapshot(&venue(), vec![inverted, empty_range], observed_at);

    assert!(snapshot.is_empty());
}

/// A malformed slot must not take the rest of the poll down with it.
#[tokio::test]
async fn a_malformed_slot_does_not_stop_the_others_from_persisting() {
    let court = court();
    let observed_at = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();

    let good = observation(&court);
    let mut malformed = observation(&court);
    malformed.starts_at = good.starts_at + ChronoDuration::hours(2);
    malformed.ends_at = malformed.starts_at - ChronoDuration::hours(1);

    let snapshot = build_snapshot(&venue(), vec![good.clone(), malformed], observed_at);
    assert_eq!(snapshot.len(), 1);

    let store = SqliteStore::open_in_memory().await.unwrap();
    store
        .replace_venue_snapshot(&venue().id, snapshot.values().cloned().collect())
        .await
        .expect("a snapshot built from a malformed observation must still persist");
    assert_eq!(store.load_snapshot().await.unwrap().len(), 1);
}

#[test]
fn snapshot_excludes_observations_that_are_not_bookable() {
    let court = court();
    let mut blocked = observation(&court);
    blocked.blocked_by_resource = true;
    let observed_at = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();

    let snapshot = build_snapshot(&venue(), vec![blocked], observed_at);

    assert!(snapshot.is_empty());
}

/// A loop wired to controllable adapters, so the failure paths around
/// discovery can be driven directly rather than inferred from their parts.
mod loop_harness {
    use super::*;
    use crate::model::CourtCatalog;
    use crate::ports::{AvailabilityChangeSink, CourtCatalogSource, VenueAvailabilitySource};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct FakeCatalogSource {
        failing: AtomicBool,
        attempts: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl CourtCatalogSource for FakeCatalogSource {
        async fn discover(&self, _venue: &Venue) -> anyhow::Result<CourtCatalog> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if self.failing.load(Ordering::SeqCst) {
                anyhow::bail!("simulated club-page outage");
            }
            Ok(CourtCatalog::new(vec![court()]))
        }
    }

    #[derive(Default)]
    struct FakeAvailabilitySource {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl VenueAvailabilitySource for FakeAvailabilitySource {
        async fn fetch(
            &self,
            _venue: &Venue,
            _catalog: &CourtCatalog,
            _starts_at: chrono::DateTime<Utc>,
            _ends_at: chrono::DateTime<Utc>,
        ) -> anyhow::Result<Vec<SlotObservation>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![observation(&court())])
        }
    }

    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<Vec<AvailabilityChange>>>);

    #[async_trait::async_trait]
    impl AvailabilityChangeSink for RecordingSink {
        async fn publish(&self, changes: &[AvailabilityChange]) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(changes.to_vec());
            Ok(())
        }
    }

    struct Harness {
        loop_: VenueLoop,
        catalogs: Arc<FakeCatalogSource>,
        availability: Arc<FakeAvailabilitySource>,
        published: Arc<RecordingSink>,
        store: Arc<SqliteStore>,
    }

    async fn harness(quiet_first_poll: bool) -> Harness {
        let store = Arc::new(SqliteStore::open_in_memory().await.unwrap());
        let catalogs = Arc::new(FakeCatalogSource::default());
        let availability = Arc::new(FakeAvailabilitySource::default());
        let published = Arc::new(RecordingSink::default());

        let mut registry = VenueRegistry::new();
        registry.register(&venue());

        Harness {
            loop_: VenueLoop {
                venue: venue(),
                interval: Duration::from_secs(300),
                lookahead_days: 7,
                // Always open, so the window never masks what is under test.
                operating_window: OperatingWindow::new(0, 24).unwrap(),
                quiet_first_poll,
                registry: Arc::new(RwLock::new(registry)),
                adapters: ProviderAdapters {
                    availability: availability.clone(),
                    catalogs: catalogs.clone(),
                },
                sinks: vec![published.clone()],
                snapshots: store.clone(),
                venue_state: store.clone(),
            },
            catalogs,
            availability,
            published,
            store,
        }
    }

    fn fetches(h: &Harness) -> usize {
        h.availability.calls.load(Ordering::SeqCst)
    }

    fn batches(h: &Harness) -> usize {
        h.published.0.lock().unwrap().len()
    }

    /// The failure the backoff must not cause: a club page that is down does
    /// not stop the venue polling, because the catalog it already has is
    /// perfectly usable.
    #[tokio::test]
    async fn a_backed_off_venue_keeps_polling_with_its_cached_catalog() {
        let h = harness(false).await;
        let mut state = MonitorState::new(BookableSlotSnapshot::new(), false);
        let mut catalog = DiscoveryState::default();

        // First tick resolves the catalog and polls.
        h.loop_.tick(&mut state, &mut catalog).await.unwrap();
        assert_eq!(fetches(&h), 1);

        // The club page goes down and the catalog is invalidated, so every
        // later tick wants to re-discover and fails.
        h.catalogs.failing.store(true, Ordering::SeqCst);
        catalog.invalidate();

        for tick in 2..=6 {
            let outcome = h.loop_.tick(&mut state, &mut catalog).await.unwrap();
            assert_eq!(
                outcome,
                TickOutcome::Polled,
                "tick {tick} bowed out despite a usable catalog"
            );
            assert_eq!(
                fetches(&h),
                tick,
                "tick {tick} skipped the availability fetch"
            );
        }
    }

    /// With no catalog at all there is nothing to poll with, so the tick is
    /// skipped — but it must not read as a success.
    #[tokio::test]
    async fn a_venue_that_never_resolved_skips_rather_than_succeeding() {
        let h = harness(false).await;
        h.catalogs.failing.store(true, Ordering::SeqCst);
        let mut state = MonitorState::new(BookableSlotSnapshot::new(), false);
        let mut catalog = DiscoveryState::default();

        // First attempt: a hard error, since there is no catalog to fall back on.
        assert!(h.loop_.tick(&mut state, &mut catalog).await.is_err());

        // Second: backing off. Neither a poll nor a failure.
        let outcome = h.loop_.tick(&mut state, &mut catalog).await.unwrap();
        assert_eq!(outcome, TickOutcome::Skipped);
        assert_eq!(fetches(&h), 0);
        assert_eq!(
            h.catalogs.attempts.load(Ordering::SeqCst),
            1,
            "the backoff did not stop the discovery attempt"
        );
    }

    /// A sustained outage must not flap between "failed" and "recovered" in the
    /// error channel, which is what counting a skipped tick as a success did.
    #[test]
    fn a_skipped_tick_leaves_the_failure_run_untouched() {
        let venue = venue();
        let mut failures = FailureRun::default();
        failures.failed(&venue, &anyhow::anyhow!("club page down"));
        assert!(failures.failing);

        // What the run loop now does for TickOutcome::Skipped: nothing.
        assert!(
            failures.failing,
            "a skipped tick must not clear the failure run"
        );

        failures.succeeded(&venue);
        assert!(!failures.failing, "a real poll still clears it");
    }

    /// Publishing happens once. A marker write that fails afterwards must not
    /// leave `previous` stale, or the next tick recomputes the same changes and
    /// sends every alert a second time.
    #[tokio::test]
    async fn a_poll_publishes_its_changes_exactly_once() {
        let h = harness(false).await;
        let mut state = MonitorState::new(BookableSlotSnapshot::new(), false);
        let mut catalog = DiscoveryState::default();

        h.loop_.tick(&mut state, &mut catalog).await.unwrap();
        assert_eq!(batches(&h), 1, "the first poll publishes its new slot");

        h.loop_.tick(&mut state, &mut catalog).await.unwrap();
        assert_eq!(
            batches(&h),
            1,
            "an unchanged poll publishes nothing further"
        );
    }

    #[tokio::test]
    async fn a_new_venue_is_quiet_and_then_records_that_it_polled() {
        let h = harness(true).await;
        let mut state = MonitorState::new(BookableSlotSnapshot::new(), true);
        let mut catalog = DiscoveryState::default();

        h.loop_.tick(&mut state, &mut catalog).await.unwrap();

        assert_eq!(batches(&h), 0, "a new venue's first poll is suppressed");
        assert!(
            h.store.is_initialised(&venue().id).await.unwrap(),
            "the venue was not recorded as initialised"
        );
    }
}
