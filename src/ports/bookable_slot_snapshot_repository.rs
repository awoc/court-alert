use anyhow::Result;
use async_trait::async_trait;

use crate::model::{BookableSlot, BookableSlotSnapshot, VenueId};

#[async_trait]
pub trait BookableSlotSnapshotRepository: Send + Sync {
    /// Every venue's slots. Used by the `/subscribe` preview, which genuinely
    /// wants all of them.
    async fn load_snapshot(&self) -> Result<BookableSlotSnapshot>;

    /// One venue's slots — a poll loop's previous state.
    ///
    /// A loop must never diff against the global snapshot: it would report
    /// every *other* venue's slots as having become unbookable, every tick.
    async fn load_venue_snapshot(&self, venue_id: &VenueId) -> Result<BookableSlotSnapshot>;

    /// Replaces one venue's slots, leaving every other venue's untouched, so a
    /// club that fails to fetch keeps its rows instead of having them deleted
    /// and re-announced on the next successful poll.
    async fn replace_venue_snapshot(
        &self,
        venue_id: &VenueId,
        slots: Vec<BookableSlot>,
    ) -> Result<()>;

    /// Drops rows belonging to venues that are no longer configured. Scoped
    /// replacement never touches them, so without this they linger forever and
    /// the subscribe preview keeps offering them.
    async fn delete_snapshots_except(&self, venue_ids: &[VenueId]) -> Result<u64>;
}
