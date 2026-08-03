use anyhow::Result;
use async_trait::async_trait;

use crate::model::{CourtCatalog, Venue};

/// Resolves which courts a venue has.
///
/// Separate from `VenueAvailabilitySource` because the two run at different
/// cadences: availability every poll, discovery once at startup and then
/// occasionally. For providers that declare their courts in config this is a
/// lookup; for Playtomic it is a scrape.
#[async_trait]
pub trait CourtCatalogSource: Send + Sync {
    async fn discover(&self, venue: &Venue) -> Result<CourtCatalog>;
}
