use anyhow::Result;
use async_trait::async_trait;

use crate::model::VenueId;

#[async_trait]
pub trait VenueStateRepository: Send + Sync {
    async fn is_initialised(&self, venue_id: &VenueId) -> Result<bool>;
    async fn mark_initialised(&self, venue_id: &VenueId) -> Result<()>;

    async fn delete_venue_state_except(&self, venue_ids: &[VenueId]) -> Result<u64>;
}
