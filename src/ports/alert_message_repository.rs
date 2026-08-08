use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::model::{AlertLine, AlertSurface, BookableSlotId, StrikePlan};

/// Tracks which alert message announced which slot, so a slot that stops being
/// bookable can be struck through in the message that announced it.
#[async_trait]
pub trait AlertMessageRepository: Send + Sync {
    async fn record_message(
        &self,
        surface: AlertSurface,
        channel_id: Option<&str>,
        message_id: &str,
        lines: &[AlertLine],
    ) -> Result<()>;

    /// Builds plans without persisting them. Lines are ordered by their stored
    /// index, and every matching message is returned.
    async fn plan_strikes(
        &self,
        surface: AlertSurface,
        slots: &[BookableSlotId],
    ) -> Result<Vec<StrikePlan>>;

    /// Persists strikes after the corresponding message edit succeeds.
    async fn commit_strikes(&self, message_id: &str, lines: &[u32]) -> Result<()>;

    async fn forget_message(&self, message_id: &str) -> Result<()>;

    /// Retains a message until all its slots have ended, since a started slot
    /// may still be bookable and need to be struck later.
    async fn prune_ended(&self, now: DateTime<Utc>) -> Result<usize>;
}
