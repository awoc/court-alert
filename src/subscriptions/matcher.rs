use std::collections::{HashMap, HashSet};

use crate::model::{
    AvailabilityChange, BookableSlot, BookableSlotId, ProviderUserRef, Schedule, Subscription,
    VenueRegistry,
};
use crate::time::local_slot_time;

pub(super) fn slot_matches(
    sub: &Subscription,
    slot: &BookableSlot,
    registry: &VenueRegistry,
) -> bool {
    if registry.sport(&slot.venue_id) != Some(sub.sport) {
        return false;
    }
    if let Some(venue) = &sub.venue
        && *venue != slot.venue_id
    {
        return false;
    }

    let local = local_slot_time(slot.starts_at);
    let matches_schedule = match &sub.schedule {
        Schedule::Weekday(w) => *w == local.weekday,
        Schedule::Date(d) => *d == local.date,
    };
    if !matches_schedule || !sub.time_range.contains(local.minute_of_day) {
        return false;
    }
    let attributes = registry.attributes_of(&slot.venue_id, slot.court_id);
    if !sub.filter.allows(attributes.as_ref()) {
        return false;
    }
    match &sub.courts {
        Some(courts) => {
            let name_lower = slot.court_name.to_lowercase();
            courts.iter().any(|c| c.to_lowercase() == name_lower)
        }
        None => true,
    }
}

pub fn match_subscriptions(
    changes: &[AvailabilityChange],
    subs: &[Subscription],
    registry: &VenueRegistry,
) -> HashMap<ProviderUserRef, Vec<BookableSlot>> {
    let mut out: HashMap<ProviderUserRef, Vec<BookableSlot>> = HashMap::new();
    let mut seen: HashMap<ProviderUserRef, HashSet<BookableSlotId>> = HashMap::new();

    for t in changes {
        let slot = match t {
            AvailabilityChange::BecameBookable(s) => s,
            AvailabilityChange::BecameUnbookable(_) => continue,
        };
        for sub in subs {
            if !slot_matches(sub, slot, registry) {
                continue;
            }
            let slot_key = BookableSlotId::from(slot);
            if seen.entry(sub.user.clone()).or_default().insert(slot_key) {
                out.entry(sub.user.clone()).or_default().push(slot.clone());
            }
        }
    }

    for slots in out.values_mut() {
        slots.sort_by_key(|s| s.starts_at);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone, Utc, Weekday};

    use super::super::testing::{
        INDOOR_COURT, OUTDOOR_COURT, SYNTHETIC_COURT, court_id, padel_court_id, padel_venue_id,
        registry, uref, venue_id,
    };
    use crate::model::{CourtFilter, CourtLocation, CourtSurface, Sport, TimeRange};

    fn slot(name: &str, hour_utc: u32, minute_utc: u32) -> BookableSlot {
        let starts_at = Utc
            .with_ymd_and_hms(2026, 6, 2, hour_utc, minute_utc, 0)
            .unwrap();
        BookableSlot {
            venue_id: venue_id(),
            court_id: court_id(name),
            court_name: name.into(),
            starts_at,
            ends_at: starts_at + chrono::Duration::hours(1),
            available_places: 1,
        }
    }

    fn padel_slot(name: &str, hour_utc: u32) -> BookableSlot {
        BookableSlot {
            venue_id: padel_venue_id(),
            court_id: padel_court_id(name),
            ..slot(name, hour_utc, 0)
        }
    }

    fn padel_sub(user: &str, venue: Option<crate::model::VenueId>) -> Subscription {
        Subscription {
            sport: Sport::Padel,
            venue,
            ..sub(user, Weekday::Tue, 18 * 60, 22 * 60, None)
        }
    }

    fn sub(
        user: &str,
        weekday: Weekday,
        from: u32,
        to: u32,
        courts: Option<Vec<&str>>,
    ) -> Subscription {
        Subscription {
            id: 1,
            user: uref(user),
            sport: Sport::Tennis,
            venue: None,
            schedule: Schedule::Weekday(weekday),
            time_range: TimeRange::new(from, to).unwrap(),
            courts: courts.map(|v| v.into_iter().map(String::from).collect()),
            filter: CourtFilter::Any,
        }
    }

    fn date_sub(user: &str, date: NaiveDate, from: u32, to: u32) -> Subscription {
        Subscription {
            id: 1,
            user: uref(user),
            sport: Sport::Tennis,
            venue: None,
            schedule: Schedule::Date(date),
            time_range: TimeRange::new(from, to).unwrap(),
            courts: None,
            filter: CourtFilter::Any,
        }
    }

    fn match_subscriptions(
        changes: &[AvailabilityChange],
        subs: &[Subscription],
    ) -> HashMap<ProviderUserRef, Vec<BookableSlot>> {
        let registry = registry();
        let registry = registry.read().unwrap();
        super::match_subscriptions(changes, subs, &registry)
    }

    #[test]
    fn empty_inputs_produce_no_matches() {
        assert!(match_subscriptions(&[], &[]).is_empty());
    }

    #[test]
    fn a_padel_subscription_for_all_clubs_never_matches_a_tennis_court() {
        let changes = vec![
            AvailabilityChange::BecameBookable(padel_slot(INDOOR_COURT, 18)),
            AvailabilityChange::BecameBookable(slot("Court 2", 18, 0)),
        ];

        let m = match_subscriptions(&changes, &[padel_sub("1", None)]);

        assert_eq!(m[&uref("1")].len(), 1);
        assert_eq!(m[&uref("1")][0].court_name, INDOOR_COURT);
    }

    #[test]
    fn a_tennis_subscription_never_matches_a_padel_court() {
        let changes = vec![AvailabilityChange::BecameBookable(padel_slot(
            INDOOR_COURT,
            18,
        ))];
        let subs = vec![sub("1", Weekday::Tue, 18 * 60, 22 * 60, None)];

        assert!(match_subscriptions(&changes, &subs).is_empty());
    }

    #[test]
    fn a_padel_subscription_with_no_location_matches_indoor_and_outdoor() {
        let changes = vec![
            AvailabilityChange::BecameBookable(padel_slot(INDOOR_COURT, 18)),
            AvailabilityChange::BecameBookable(padel_slot(OUTDOOR_COURT, 19)),
        ];

        let m = match_subscriptions(&changes, &[padel_sub("1", None)]);

        assert_eq!(m[&uref("1")].len(), 2);
    }

    #[test]
    fn a_location_filter_narrows_a_padel_subscription() {
        let changes = vec![
            AvailabilityChange::BecameBookable(padel_slot(INDOOR_COURT, 18)),
            AvailabilityChange::BecameBookable(padel_slot(OUTDOOR_COURT, 19)),
        ];
        let indoor = Subscription {
            filter: CourtFilter::Location(CourtLocation::Indoor),
            ..padel_sub("1", None)
        };

        let m = match_subscriptions(&changes, &[indoor]);

        assert_eq!(m[&uref("1")].len(), 1);
        assert_eq!(m[&uref("1")][0].court_name, INDOOR_COURT);
    }

    #[test]
    fn naming_a_club_excludes_every_other_club() {
        let changes = vec![AvailabilityChange::BecameBookable(padel_slot(
            INDOOR_COURT,
            18,
        ))];

        let matching = match_subscriptions(&changes, &[padel_sub("1", Some(padel_venue_id()))]);
        assert_eq!(matching[&uref("1")].len(), 1);

        let elsewhere = match_subscriptions(
            &changes,
            &[padel_sub("1", Some(crate::model::VenueId::new("other")))],
        );
        assert!(elsewhere.is_empty());
    }

    #[test]
    fn a_court_filter_excludes_courts_of_other_surfaces() {
        let changes = vec![
            AvailabilityChange::BecameBookable(slot("Court 2", 18, 0)),
            AvailabilityChange::BecameBookable(slot(SYNTHETIC_COURT, 18, 0)),
        ];
        let mut clay = sub("1", Weekday::Tue, 18 * 60, 22 * 60, None);
        clay.filter = CourtFilter::CLAY;
        let mut synthetic = sub("2", Weekday::Tue, 18 * 60, 22 * 60, None);
        synthetic.filter = CourtFilter::Surface(CourtSurface::Synthetic);

        let m = match_subscriptions(&changes, &[clay, synthetic]);

        assert_eq!(m[&uref("1")].len(), 1);
        assert_eq!(m[&uref("1")][0].court_name, "Court 2");
        assert_eq!(m[&uref("2")].len(), 1);
        assert_eq!(m[&uref("2")][0].court_name, SYNTHETIC_COURT);
    }

    #[test]
    fn the_any_filter_keeps_every_surface() {
        let changes = vec![
            AvailabilityChange::BecameBookable(slot("Court 2", 18, 0)),
            AvailabilityChange::BecameBookable(slot(SYNTHETIC_COURT, 18, 0)),
        ];
        let subs = vec![sub("1", Weekday::Tue, 18 * 60, 22 * 60, None)];
        assert_eq!(match_subscriptions(&changes, &subs)[&uref("1")].len(), 2);
    }

    #[test]
    fn a_court_filter_excludes_unconfigured_courts() {
        let changes = vec![AvailabilityChange::BecameBookable(slot(
            "Retired court",
            18,
            0,
        ))];
        let mut clay = sub("1", Weekday::Tue, 18 * 60, 22 * 60, None);
        clay.filter = CourtFilter::CLAY;
        assert!(match_subscriptions(&changes, &[clay]).is_empty());
    }

    #[test]
    fn ignores_becameunbookable_changes() {
        let s = slot("Court 2", 18, 0);
        let changes = vec![AvailabilityChange::BecameUnbookable(s)];
        let subs = vec![sub("1", Weekday::Tue, 18 * 60, 22 * 60, None)];
        assert!(match_subscriptions(&changes, &subs).is_empty());
    }

    #[test]
    fn matches_weekday_and_time_range() {
        let changes = vec![AvailabilityChange::BecameBookable(slot("Court 2", 18, 0))];
        let subs = vec![sub("1", Weekday::Tue, 18 * 60, 22 * 60, None)];
        let m = match_subscriptions(&changes, &subs);
        assert_eq!(m.len(), 1);
        assert_eq!(m[&uref("1")].len(), 1);
    }

    #[test]
    fn wrong_weekday_excluded() {
        let changes = vec![AvailabilityChange::BecameBookable(slot("Court 2", 18, 0))];
        let subs = vec![sub("1", Weekday::Wed, 0, 24 * 60, None)];
        assert!(match_subscriptions(&changes, &subs).is_empty());
    }

    #[test]
    fn time_range_is_half_open() {
        let at_18 = slot("Court 2", 16, 0); // 18:00 CEST
        let at_20 = slot("Court 2", 18, 0);
        let subs = vec![sub("1", Weekday::Tue, 18 * 60, 20 * 60, None)];

        let included = match_subscriptions(&[AvailabilityChange::BecameBookable(at_18)], &subs);
        assert_eq!(included[&uref("1")].len(), 1);

        let excluded = match_subscriptions(&[AvailabilityChange::BecameBookable(at_20)], &subs);
        assert!(excluded.is_empty());
    }

    #[test]
    fn court_filter_excludes_non_matching_courts() {
        let changes = vec![
            AvailabilityChange::BecameBookable(slot("Court 2", 18, 0)),
            AvailabilityChange::BecameBookable(slot("Court 5", 18, 0)),
        ];
        let subs = vec![sub(
            "1",
            Weekday::Tue,
            18 * 60,
            22 * 60,
            Some(vec!["Court 2"]),
        )];
        let m = match_subscriptions(&changes, &subs);
        assert_eq!(m[&uref("1")].len(), 1);
        assert_eq!(m[&uref("1")][0].court_name, "Court 2");
    }

    #[test]
    fn court_filter_is_case_insensitive() {
        let changes = vec![AvailabilityChange::BecameBookable(slot("Court 2", 18, 0))];
        let subs = vec![sub(
            "1",
            Weekday::Tue,
            18 * 60,
            22 * 60,
            Some(vec!["court 2"]),
        )];
        let m = match_subscriptions(&changes, &subs);
        assert_eq!(m[&uref("1")].len(), 1);
    }

    #[test]
    fn dedups_when_multiple_subs_match_same_slot_for_same_user() {
        let changes = vec![AvailabilityChange::BecameBookable(slot("Court 2", 18, 0))];
        let subs = vec![
            sub("1", Weekday::Tue, 18 * 60, 22 * 60, None),
            sub("1", Weekday::Tue, 19 * 60, 21 * 60, None),
        ];
        let m = match_subscriptions(&changes, &subs);
        assert_eq!(m[&uref("1")].len(), 1);
    }

    #[test]
    fn different_users_get_separate_entries() {
        let changes = vec![AvailabilityChange::BecameBookable(slot("Court 2", 18, 0))];
        let subs = vec![
            sub("1", Weekday::Tue, 18 * 60, 22 * 60, None),
            sub("2", Weekday::Tue, 18 * 60, 22 * 60, None),
        ];
        let m = match_subscriptions(&changes, &subs);
        assert_eq!(m.len(), 2);
        assert!(m.contains_key(&uref("1")));
        assert!(m.contains_key(&uref("2")));
    }

    #[test]
    fn slots_in_output_are_sorted_chronologically() {
        let changes = vec![
            AvailabilityChange::BecameBookable(slot("Court 2", 20, 0)),
            AvailabilityChange::BecameBookable(slot("Court 2", 18, 0)),
            AvailabilityChange::BecameBookable(slot("Court 2", 19, 0)),
        ];
        let subs = vec![sub("1", Weekday::Tue, 0, 24 * 60, None)];
        let m = match_subscriptions(&changes, &subs);
        let slots = &m[&uref("1")];
        assert!(slots[0].starts_at < slots[1].starts_at);
        assert!(slots[1].starts_at < slots[2].starts_at);
    }

    #[test]
    fn date_subscription_matches_slot_on_that_berlin_date() {
        let changes = vec![AvailabilityChange::BecameBookable(slot("Court 2", 18, 0))];
        let subs = vec![date_sub(
            "1",
            NaiveDate::from_ymd_opt(2026, 6, 2).unwrap(),
            18 * 60,
            22 * 60,
        )];
        let m = match_subscriptions(&changes, &subs);
        assert_eq!(m[&uref("1")].len(), 1);
    }

    #[test]
    fn date_subscription_excludes_other_dates() {
        let changes = vec![AvailabilityChange::BecameBookable(slot("Court 2", 18, 0))];
        let subs = vec![date_sub(
            "1",
            NaiveDate::from_ymd_opt(2026, 6, 9).unwrap(), // a week later
            18 * 60,
            22 * 60,
        )];
        assert!(match_subscriptions(&changes, &subs).is_empty());
    }
}
