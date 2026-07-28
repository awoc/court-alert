use tracing::info;

use crate::model::AvailabilityChange;
use crate::time::fmt_berlin_log;

pub(super) fn log_changes(changes: &[AvailabilityChange], total_bookable: usize) {
    let (added, removed) = changes
        .iter()
        .fold((0usize, 0usize), |counts, change| match change {
            AvailabilityChange::BecameBookable(_) => (counts.0 + 1, counts.1),
            AvailabilityChange::BecameUnbookable(_) => (counts.0, counts.1 + 1),
        });

    info!(added, removed, bookable = total_bookable, "tick");

    let mut chronological = changes.iter().collect::<Vec<_>>();
    chronological.sort_by_key(|change| change.slot().starts_at);

    for change in chronological {
        match change {
            AvailabilityChange::BecameBookable(slot) => info!(
                court = %slot.court_name,
                start = %fmt_berlin_log(slot.starts_at),
                end = %fmt_berlin_log(slot.ends_at),
                "+ became bookable"
            ),
            AvailabilityChange::BecameUnbookable(slot) => info!(
                court = %slot.court_name,
                start = %fmt_berlin_log(slot.starts_at),
                "- became unbookable"
            ),
        }
    }
}
