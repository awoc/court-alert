use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::model::{Court, SlotObservation, Venue};

#[async_trait]
pub trait SlotAvailabilitySource: Send + Sync {
    async fn fetch(
        &self,
        venue: &Venue,
        court: &Court,
        starts_at: DateTime<Utc>,
        ends_at: DateTime<Utc>,
    ) -> Result<Vec<SlotObservation>>;
}
