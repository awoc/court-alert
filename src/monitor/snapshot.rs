use chrono::{DateTime, Utc};
use tracing::{debug, warn};

use crate::model::{BookableSlotId, BookableSlotSnapshot, SlotObservation, Venue};
use crate::time::fmt_berlin_log;

pub(super) fn build_snapshot(
    venue: &Venue,
    observations: Vec<SlotObservation>,
    observed_at: DateTime<Utc>,
) -> BookableSlotSnapshot {
    let observed_count = observations.len();
    let mut snapshot = BookableSlotSnapshot::new();

    for observation in observations {
        if observation.ends_at <= observation.starts_at {
            warn!(
                venue = %venue.id,
                court = %observation.court_name,
                court_id = %observation.court_id,
                start = %fmt_berlin_log(observation.starts_at),
                end = %fmt_berlin_log(observation.ends_at),
                "provider returned a slot that does not end after it starts; skipping"
            );
            continue;
        }

        let Some(slot) = observation.into_bookable(observed_at) else {
            continue;
        };

        let court_name = slot.court_name.clone();
        let court_id = slot.court_id;
        let id = BookableSlotId::from(&slot);
        if snapshot.insert(id, slot).is_some() {
            warn!(
                venue = %venue.id,
                court = %court_name,
                %court_id,
                start = %fmt_berlin_log(id.starts_at),
                "provider returned a duplicate slot; keeping the last observation"
            );
        }
    }

    debug!(
        venue = %venue.id,
        observed_slots = observed_count,
        bookable_slots = snapshot.len(),
        "venue polled"
    );

    snapshot
}
