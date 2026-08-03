use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::model::{CourtCatalog, SlotObservation, Venue};

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
