use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::model::{CourtCatalog, SlotObservation, Venue};

/// Fetches one venue's availability for a window.
///
/// Venue granularity rather than per-court: ZHS is naturally per-product and
/// fans out internally, while a Playtomic request returns *every* resource for
/// a date, so per-court fetching would refetch the same payload once per court.
///
/// The catalog is a parameter rather than something the source reads from the
/// registry, so no lock is held across the `await`.
#[async_trait]
pub trait VenueAvailabilitySource: Send + Sync {
    async fn fetch(
        &self,
        venue: &Venue,
        catalog: &CourtCatalog,
        starts_at: DateTime<Utc>,
        ends_at: DateTime<Utc>,
    ) -> Result<Vec<SlotObservation>>;
}
