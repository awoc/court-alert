use std::sync::{Arc, RwLock};

use anyhow::Result;
use async_trait::async_trait;

use crate::model::{AvailabilityChange, Sport, VenueRegistry};
use crate::ports::AvailabilityChangeSink;

pub struct SportScopedSink {
    inner: Box<dyn AvailabilityChangeSink>,
    registry: Arc<RwLock<VenueRegistry>>,
    sport: Sport,
}

impl SportScopedSink {
    pub fn wrap(
        inner: Box<dyn AvailabilityChangeSink>,
        registry: Arc<RwLock<VenueRegistry>>,
        sport: Sport,
    ) -> Box<dyn AvailabilityChangeSink> {
        Box::new(Self {
            inner,
            registry,
            sport,
        })
    }
}

#[async_trait]
impl AvailabilityChangeSink for SportScopedSink {
    async fn publish(&self, changes: &[AvailabilityChange]) -> Result<()> {
        let kept: Vec<AvailabilityChange> = {
            let registry = self.registry.read().expect("venue registry poisoned");
            changes
                .iter()
                .filter(|change| registry.sport(&change.slot().venue_id) == Some(self.sport))
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
        BookableSlot, Court, CourtAttributes, CourtCatalog, CourtFilter, CourtLocation,
        CourtSurface, Venue, VenueId, VenueIdentity,
    };
    use crate::notify::SurfaceFilteredSink;
    use chrono::{TimeZone, Utc};
    use std::sync::Mutex;
    use uuid::Uuid;

    const CLAY_ID: Uuid = Uuid::from_u128(2);
    const PADEL_ID: Uuid = Uuid::from_u128(101);

    fn tennis_id() -> VenueId {
        VenueId::new("zhs-munich")
    }

    fn padel_id() -> VenueId {
        VenueId::new("casa-padel")
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

    fn venue(id: VenueId, sport: Sport) -> Venue {
        Venue {
            id,
            display_name: "A Club".into(),
            sport,
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
        registry.register(&venue(tennis_id(), Sport::Tennis));
        registry.set_catalog(
            &tennis_id(),
            CourtCatalog::new(vec![Court::new(
                CLAY_ID,
                "Court 2".into(),
                CourtAttributes::tennis(CourtSurface::Clay),
            )]),
        );
        registry.register(&venue(padel_id(), Sport::Padel));
        registry.set_catalog(
            &padel_id(),
            CourtCatalog::new(vec![Court::new(
                PADEL_ID,
                "Court 1 (Indoor)".into(),
                CourtAttributes::padel(Some(CourtLocation::Indoor)),
            )]),
        );
        Arc::new(RwLock::new(registry))
    }

    fn slot(venue_id: VenueId, court_id: Uuid, court_name: &str) -> BookableSlot {
        let starts_at = Utc.with_ymd_and_hms(2026, 6, 2, 18, 0, 0).unwrap();
        BookableSlot {
            venue_id,
            court_id,
            court_name: court_name.into(),
            starts_at,
            ends_at: starts_at + chrono::Duration::hours(1),
            available_places: 1,
        }
    }

    fn tennis_slot() -> AvailabilityChange {
        AvailabilityChange::BecameBookable(slot(tennis_id(), CLAY_ID, "Court 2"))
    }

    fn padel_slot() -> AvailabilityChange {
        AvailabilityChange::BecameBookable(slot(padel_id(), PADEL_ID, "Court 1 (Indoor)"))
    }

    fn published(recorder: &Arc<RecordingSink>) -> Vec<Vec<AvailabilityChange>> {
        recorder.0.lock().unwrap().clone()
    }

    fn broadcast_chain(
        filter: CourtFilter,
    ) -> (Box<dyn AvailabilityChangeSink>, Arc<RecordingSink>) {
        let recorder = Arc::new(RecordingSink::default());
        let registry = registry();
        let sink = SportScopedSink::wrap(
            SurfaceFilteredSink::wrap(Box::new(recorder.clone()), registry.clone(), filter),
            registry,
            Sport::Tennis,
        );
        (sink, recorder)
    }

    #[tokio::test]
    async fn padel_never_reaches_the_broadcast_channel_even_with_the_any_filter() {
        for filter in [CourtFilter::Any, CourtFilter::CLAY] {
            let (sink, recorder) = broadcast_chain(filter);

            sink.publish(&[padel_slot()]).await.unwrap();

            assert!(
                published(&recorder).is_empty(),
                "a padel slot reached the tennis webhook with filter {filter}"
            );
        }
    }

    #[tokio::test]
    async fn tennis_still_reaches_the_broadcast_channel() {
        let (sink, recorder) = broadcast_chain(CourtFilter::Any);

        sink.publish(&[tennis_slot()]).await.unwrap();

        assert_eq!(published(&recorder), vec![vec![tennis_slot()]]);
    }

    #[tokio::test]
    async fn a_mixed_batch_is_narrowed_to_the_scoped_sport() {
        let (sink, recorder) = broadcast_chain(CourtFilter::Any);

        sink.publish(&[padel_slot(), tennis_slot()]).await.unwrap();

        assert_eq!(published(&recorder), vec![vec![tennis_slot()]]);
    }

    #[tokio::test]
    async fn a_slot_from_an_unregistered_venue_is_dropped() {
        let (sink, recorder) = broadcast_chain(CourtFilter::Any);
        let mut orphan = slot(VenueId::new("gone"), CLAY_ID, "Court 2");
        orphan.court_name = "Court 2".into();

        sink.publish(&[AvailabilityChange::BecameBookable(orphan)])
            .await
            .unwrap();

        assert!(published(&recorder).is_empty());
    }
}
