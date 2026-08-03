use anyhow::Result;
use async_trait::async_trait;

use crate::model::VenueId;

/// Remembers which venues have completed a poll.
///
/// Recorded rather than inferred from an empty snapshot: a single club can
/// legitimately be fully booked across its whole horizon, and inferring would
/// then swallow its first batch of freed slots after a restart.
#[async_trait]
pub trait VenueStateRepository: Send + Sync {
    async fn is_initialised(&self, venue_id: &VenueId) -> Result<bool>;
    async fn mark_initialised(&self, venue_id: &VenueId) -> Result<()>;

    /// Forgets venues that are no longer configured.
    ///
    /// Must be swept alongside the slots: a venue whose rows were dropped but
    /// whose marker survived would, if re-added later, diff an empty snapshot
    /// against its whole horizon and announce every free slot at once.
    async fn delete_venue_state_except(&self, venue_ids: &[VenueId]) -> Result<u64>;
}
