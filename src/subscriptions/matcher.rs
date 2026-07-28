use std::collections::{HashMap, HashSet};

use crate::model::{
    AvailabilityChange, BookableSlot, BookableSlotId, ProviderUserRef, Schedule, Subscription,
};
use crate::time::local_slot_time;

pub(super) fn slot_matches(sub: &Subscription, slot: &BookableSlot) -> bool {
    let local = local_slot_time(slot.starts_at);
    let matches_schedule = match &sub.schedule {
        Schedule::Weekday(w) => *w == local.weekday,
        Schedule::Date(d) => *d == local.date,
    };
    if !matches_schedule || !sub.time_range.contains(local.minute_of_day) {
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
) -> HashMap<ProviderUserRef, Vec<BookableSlot>> {
    let mut out: HashMap<ProviderUserRef, Vec<BookableSlot>> = HashMap::new();
    let mut seen: HashMap<ProviderUserRef, HashSet<BookableSlotId>> = HashMap::new();

    for t in changes {
        let slot = match t {
            AvailabilityChange::BecameBookable(s) => s,
            AvailabilityChange::BecameUnbookable(_) => continue,
        };
        for sub in subs {
            if !slot_matches(sub, slot) {
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
    use uuid::Uuid;

    use crate::model::TimeRange;

    fn uref(id: &str) -> ProviderUserRef {
        ProviderUserRef {
            provider: "discord".into(),
            user_id: id.into(),
        }
    }

    fn slot(name: &str, hour_utc: u32, minute_utc: u32) -> BookableSlot {
        let starts_at = Utc
            .with_ymd_and_hms(2026, 6, 2, hour_utc, minute_utc, 0)
            .unwrap();
        BookableSlot {
            court_id: Uuid::nil(),
            court_name: name.into(),
            starts_at,
            ends_at: starts_at + chrono::Duration::hours(1),
            available_places: 1,
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
            schedule: Schedule::Weekday(weekday),
            time_range: TimeRange::new(from, to).unwrap(),
            courts: courts.map(|v| v.into_iter().map(String::from).collect()),
        }
    }

    fn date_sub(user: &str, date: NaiveDate, from: u32, to: u32) -> Subscription {
        Subscription {
            id: 1,
            user: uref(user),
            schedule: Schedule::Date(date),
            time_range: TimeRange::new(from, to).unwrap(),
            courts: None,
        }
    }

    #[test]
    fn empty_inputs_produce_no_matches() {
        assert!(match_subscriptions(&[], &[]).is_empty());
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
