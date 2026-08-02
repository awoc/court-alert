use chrono::{Duration as ChronoDuration, TimeZone, Utc};

use super::*;

use crate::model::{
    Court, CourtAttributes, CourtSurface, SlotObservation, Sport, Venue, VenueId, VenueIdentity,
};
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

#[test]
fn quiet_first_poll_is_used_only_for_an_empty_initial_snapshot() {
    let empty = MonitorState::new(BookableSlotSnapshot::new(), true);
    assert!(empty.suppress_next_publish);

    let court = court();
    let observed_at = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
    let populated = build_snapshot(
        vec![(&venue(), court.clone(), vec![observation(&court)])],
        observed_at,
    );
    let restored = MonitorState::new(populated, true);
    assert!(!restored.suppress_next_publish);
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

    let snapshot = build_snapshot(
        vec![(&venue(), court.clone(), vec![first, duplicate])],
        observed_at,
    );

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

    let snapshot = build_snapshot(
        vec![(&venue(), court.clone(), vec![inverted, empty_range])],
        observed_at,
    );

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

    let snapshot = build_snapshot(
        vec![(&venue(), court.clone(), vec![good.clone(), malformed])],
        observed_at,
    );
    assert_eq!(snapshot.len(), 1);

    let store = SqliteStore::open_in_memory().await.unwrap();
    store
        .replace_snapshot(snapshot.values().cloned().collect())
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

    let snapshot = build_snapshot(vec![(&venue(), court.clone(), vec![blocked])], observed_at);

    assert!(snapshot.is_empty());
}
