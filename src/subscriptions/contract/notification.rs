use anyhow::Result;
use async_trait::async_trait;

use super::AvailableSlotSummary;
use crate::model::BookableSlotId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailabilityAlert {
    pub slots: Vec<AvailableSlotSummary>,
}

#[async_trait]
pub trait AlertSender: Send + Sync {
    async fn send_alert(&self, user_id: &str, alert: &AvailabilityAlert) -> Result<()>;

    async fn strike_taken(&self, _slots: &[BookableSlotId]) -> Result<()> {
        Ok(())
    }
}
