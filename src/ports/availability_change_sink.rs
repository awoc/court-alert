use anyhow::Result;
use async_trait::async_trait;

use crate::model::AvailabilityChange;

#[async_trait]
pub trait AvailabilityChangeSink: Send + Sync {
    async fn publish(&self, changes: &[AvailabilityChange]) -> Result<()>;
}
