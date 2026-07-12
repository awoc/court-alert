use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::config::MonitoredCourt;
use crate::domain::SlotObservation;

#[async_trait]
pub trait SlotAvailabilitySource: Send + Sync {
    async fn fetch(
        &self,
        court: &MonitoredCourt,
        starts_at: DateTime<Utc>,
        ends_at: DateTime<Utc>,
    ) -> Result<Vec<SlotObservation>>;
}
