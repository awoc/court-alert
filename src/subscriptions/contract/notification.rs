use anyhow::Result;
use async_trait::async_trait;

use super::AvailableSlotSummary;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailabilityAlert {
    pub slots: Vec<AvailableSlotSummary>,
}

#[async_trait]
pub trait DirectMessageSender: Send + Sync {
    async fn send_dm(&self, user_id: &str, alert: &AvailabilityAlert) -> Result<()>;
}
