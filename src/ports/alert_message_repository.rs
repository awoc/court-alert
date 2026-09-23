use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::model::{AlertLine, AlertMessageKey, AlertSurface, BookableSlotId, StrikePlan};

/// Stores announced slots and their state under each complete alert message key.
#[async_trait]
pub trait AlertMessageRepository: Send + Sync {
    async fn record_message(&self, key: &AlertMessageKey, lines: &[AlertLine]) -> Result<()>;

    /// Builds plans without persisting them. Lines are ordered by their stored
    /// index, and every matching message within the chat provider and surface is returned.
    async fn plan_strikes(
        &self,
        chat_provider: &str,
        surface: AlertSurface,
        slots: &[BookableSlotId],
    ) -> Result<Vec<StrikePlan>>;

    /// Persists strikes after the corresponding message edit succeeds.
    async fn commit_strikes(&self, key: &AlertMessageKey, lines: &[u32]) -> Result<()>;

    async fn forget_message(&self, key: &AlertMessageKey) -> Result<()>;

    /// Retains a message until all its slots have ended, since a started slot
    /// may still be bookable and need to be struck later.
    async fn prune_ended(&self, now: DateTime<Utc>) -> Result<usize>;
}
