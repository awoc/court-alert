use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::model::{AlertLine, BookableSlotId, StrikePlan};

/// Tracks which alert message announced which slot, so a slot that stops being
/// bookable can be struck through in the message that announced it.
#[async_trait]
pub trait AlertMessageRepository: Send + Sync {
    /// Records the lines of a freshly posted message. `line_index` is the
    /// position in `lines`, which must be the order they were rendered in.
    async fn record_message(&self, message_id: &str, lines: &[AlertLine]) -> Result<()>;

    /// Returns a plan for each message holding an unstruck line for one of
    /// `slots`. Read-only: nothing is persisted until `commit_strikes`.
    ///
    /// Every plan's `message.lines` is ordered by `line_index` ascending.
    /// `AlertLine` carries no index, so rendering relies entirely on this.
    ///
    /// A single slot can appear unstruck in more than one message — a slot
    /// whose edit failed and which later became bookable again is announced
    /// afresh. Every such message is returned, which is how the stale one is
    /// repaired.
    async fn plan_strikes(&self, slots: &[BookableSlotId]) -> Result<Vec<StrikePlan>>;

    /// Persists the planned strikes, after Discord confirmed the edit. Called
    /// with a plan's `message.id` and its `newly_struck`.
    async fn commit_strikes(&self, message_id: &str, lines: &[u32]) -> Result<()>;

    /// Drops all rows of a message that no longer exists in Discord.
    async fn forget_message(&self, message_id: &str) -> Result<()>;

    /// Drops messages whose slots have all ended — the first moment nothing
    /// about them can change again. Returns the number of rows removed.
    ///
    /// Retention runs to `ends_at` rather than `starts_at` because a slot with
    /// no booking deadline stays bookable after it has started: its removal can
    /// arrive mid-slot, and the row has to still be there to be struck.
    async fn prune_ended(&self, now: DateTime<Utc>) -> Result<usize>;
}
