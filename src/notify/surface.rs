use std::sync::{Arc, RwLock};

use anyhow::Result;
use async_trait::async_trait;

use crate::model::{AvailabilityChange, CourtFilter, VenueRegistry};
use crate::ports::AvailabilityChangeSink;

pub struct SurfaceFilteredSink {
    inner: Box<dyn AvailabilityChangeSink>,
    registry: Arc<RwLock<VenueRegistry>>,
    filter: CourtFilter,
}

impl SurfaceFilteredSink {
    pub fn wrap(
        inner: Box<dyn AvailabilityChangeSink>,
        registry: Arc<RwLock<VenueRegistry>>,
        filter: CourtFilter,
    ) -> Box<dyn AvailabilityChangeSink> {
        match filter {
            CourtFilter::Any => inner,
            filter => Box::new(Self {
                inner,
                registry,
                filter,
            }),
        }
    }
}

#[async_trait]
impl AvailabilityChangeSink for SurfaceFilteredSink {
    async fn publish(&self, changes: &[AvailabilityChange]) -> Result<()> {
        let kept: Vec<AvailabilityChange> = {
            let registry = self.registry.read().expect("venue registry poisoned");
            changes
                .iter()
                .filter(|change| {
                    let slot = change.slot();
                    let attributes = registry.attributes_of(&slot.venue_id, slot.court_id);
                    self.filter.allows(attributes.as_ref())
                })
                .cloned()
                .collect()
        };
        if kept.is_empty() {
            return Ok(());
        }
        self.inner.publish(&kept).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        BookableSlot, Court, CourtAttributes, CourtCatalog, CourtSurface, Sport, Venue, VenueId,
        VenueIdentity,
    };
    use chrono::{TimeZone, Utc};
    use std::sync::Mutex;
    use uuid::Uuid;

    const CLAY_ID: Uuid = Uuid::from_u128(2);
    const SYNTHETIC_ID: Uuid = Uuid::from_u128(19);

    fn venue_id() -> VenueId {
        VenueId::new("zhs-munich")
    }

    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<Vec<AvailabilityChange>>>);

    #[async_trait]
    impl AvailabilityChangeSink for Arc<RecordingSink> {
        async fn publish(&self, changes: &[AvailabilityChange]) -> Result<()> {
            self.0.lock().unwrap().push(changes.to_vec());
            Ok(())
        }
    }

    fn venue() -> Venue {
        Venue {
            id: venue_id(),
            display_name: "ZHS München".into(),
            sport: Sport::Tennis,
            identity: VenueIdentity::Zhs {
                base_url: "https://example.test".into(),
            },
            poll_interval_secs: None,
            lookahead_days: None,
            operating_window: None,
        }
    }

    fn registry() -> Arc<RwLock<VenueRegistry>> {
        let mut registry = VenueRegistry::new();
        registry.register(&venue());
        registry.set_catalog(
            &venue_id(),
            CourtCatalog::new(vec![
                Court::new(
                    CLAY_ID,
                    "Court 2".into(),
                    CourtAttributes::tennis(CourtSurface::Clay),
                ),
                Court::new(
                    SYNTHETIC_ID,
                    "Court 19 - Synthetic".into(),
                    CourtAttributes::tennis(CourtSurface::Synthetic),
                ),
            ]),
        );
        Arc::new(RwLock::new(registry))
    }

    fn slot(court_id: Uuid, court_name: &str) -> BookableSlot {
        let starts_at = Utc.with_ymd_and_hms(2026, 6, 2, 18, 0, 0).unwrap();
        BookableSlot {
            venue_id: venue_id(),
            court_id,
            court_name: court_name.into(),
            starts_at,
            ends_at: starts_at + chrono::Duration::hours(1),
            available_places: 1,
        }
    }

    fn sink(filter: CourtFilter) -> (Box<dyn AvailabilityChangeSink>, Arc<RecordingSink>) {
        let recorder = Arc::new(RecordingSink::default());
        (
            SurfaceFilteredSink::wrap(Box::new(recorder.clone()), registry(), filter),
            recorder,
        )
    }

    fn published(recorder: &Arc<RecordingSink>) -> Vec<Vec<AvailabilityChange>> {
        recorder.0.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn only_matching_surfaces_reach_the_inner_sink() {
        let (sink, recorder) = sink(CourtFilter::CLAY);

        sink.publish(&[
            AvailabilityChange::BecameBookable(slot(CLAY_ID, "Court 2")),
            AvailabilityChange::BecameBookable(slot(SYNTHETIC_ID, "Court 19 - Synthetic")),
        ])
        .await
        .unwrap();

        let batches = published(&recorder);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[0][0].slot().court_name, "Court 2");
    }

    #[tokio::test]
    async fn removals_are_filtered_the_same_way() {
        let (sink, recorder) = sink(CourtFilter::CLAY);

        sink.publish(&[AvailabilityChange::BecameUnbookable(slot(
            SYNTHETIC_ID,
            "Court 19 - Synthetic",
        ))])
        .await
        .unwrap();

        assert!(published(&recorder).is_empty());
    }

    #[tokio::test]
    async fn a_batch_with_nothing_left_is_not_forwarded() {
        let (sink, recorder) = sink(CourtFilter::Surface(CourtSurface::Synthetic));

        sink.publish(&[AvailabilityChange::BecameBookable(slot(CLAY_ID, "Court 2"))])
            .await
            .unwrap();

        assert!(published(&recorder).is_empty());
    }

    #[tokio::test]
    async fn the_any_filter_forwards_everything_unchanged() {
        let (sink, recorder) = sink(CourtFilter::Any);
        let changes = [
            AvailabilityChange::BecameBookable(slot(CLAY_ID, "Court 2")),
            AvailabilityChange::BecameBookable(slot(Uuid::nil(), "Retired court")),
        ];

        sink.publish(&changes).await.unwrap();

        assert_eq!(published(&recorder), vec![changes.to_vec()]);
    }

    #[tokio::test]
    async fn slots_of_unconfigured_courts_are_dropped() {
        let (sink, recorder) = sink(CourtFilter::CLAY);

        sink.publish(&[AvailabilityChange::BecameBookable(slot(
            Uuid::nil(),
            "Retired court",
        ))])
        .await
        .unwrap();

        assert!(published(&recorder).is_empty());
    }

    #[tokio::test]
    async fn slots_from_an_unregistered_venue_are_dropped() {
        let (sink, recorder) = sink(CourtFilter::CLAY);
        let mut foreign = slot(CLAY_ID, "Court 2");
        foreign.venue_id = VenueId::new("elsewhere");

        sink.publish(&[AvailabilityChange::BecameBookable(foreign)])
            .await
            .unwrap();

        assert!(published(&recorder).is_empty());
    }
}
