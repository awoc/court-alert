use chrono::{DateTime, Utc};
use tracing::{debug, warn};

use crate::model::{BookableSlotId, BookableSlotSnapshot, Court, SlotObservation, Venue};
use crate::time::fmt_berlin_log;

pub(super) fn build_snapshot(
    fetched: Vec<(&Venue, Court, Vec<SlotObservation>)>,
    observed_at: DateTime<Utc>,
) -> BookableSlotSnapshot {
    let mut snapshot = BookableSlotSnapshot::new();

    for (venue, court, observations) in fetched {
        let observed_count = observations.len();
        let mut bookable_count = 0;

        for observation in observations {
            if observation.ends_at <= observation.starts_at {
                warn!(
                    venue = %venue.id,
                    court = %court.name(),
                    court_id = %court.id(),
                    start = %fmt_berlin_log(observation.starts_at),
                    end = %fmt_berlin_log(observation.ends_at),
                    "provider returned a slot that does not end after it starts; skipping"
                );
                continue;
            }

            let Some(slot) = observation.into_bookable(observed_at) else {
                continue;
            };

            let id = BookableSlotId::from(&slot);
            if snapshot.insert(id, slot).is_none() {
                bookable_count += 1;
            } else {
                warn!(
                    venue = %venue.id,
                    court = %court.name(),
                    court_id = %court.id(),
                    start = %fmt_berlin_log(id.starts_at),
                    "provider returned a duplicate slot; keeping the last observation"
                );
            }
        }

        debug!(
            venue = %venue.id,
            court = %court.name(),
            court_id = %court.id(),
            observed_slots = observed_count,
            bookable_slots = bookable_count,
            "court polled"
        );
    }

    snapshot
}
