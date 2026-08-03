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
    let mut state = CatalogState::default();
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
    let mut state = CatalogState::default();
    for _ in 0..20 {
        state.failed();
    }
    assert_eq!(state.ticks_to_skip, MAX_DISCOVERY_BACKOFF_TICKS);
}

/// Startup-only discovery goes stale: a club renames or adds a court and
/// nobody tells us.
#[test]
fn a_resolved_catalog_is_fresh_until_the_refresh_interval() {
    let mut state = CatalogState::default();
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
    let mut state = CatalogState::default();
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
