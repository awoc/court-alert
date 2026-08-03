use anyhow::Result;
use async_trait::async_trait;

use crate::model::{BookableSlot, BookableSlotSnapshot, VenueId};

#[async_trait]
pub trait BookableSlotSnapshotRepository: Send + Sync {
    async fn load_snapshot(&self) -> Result<BookableSlotSnapshot>;

    async fn load_venue_snapshot(&self, venue_id: &VenueId) -> Result<BookableSlotSnapshot>;

    async fn replace_venue_snapshot(
        &self,
        venue_id: &VenueId,
        slots: Vec<BookableSlot>,
    ) -> Result<()>;

    async fn delete_snapshots_except(&self, venue_ids: &[VenueId]) -> Result<u64>;
}
