//! How availability changes reach the outside world.
//!
//! Only the parts that hold for every destination: the channel into the
//! subscription dispatcher, and the decorators that narrow what a broadcast
//! carries. Anything that knows a particular service lives with that service —
//! Discord's webhook is in [`crate::chat::discord`].

mod sport;
mod surface;

pub use sport::SportScopedSink;
pub use surface::SurfaceFilteredSink;

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::model::AvailabilityChange;
use crate::ports::AvailabilityChangeSink;

pub struct ChannelSink(tokio::sync::mpsc::Sender<Vec<AvailabilityChange>>);

impl ChannelSink {
    pub fn new(sender: tokio::sync::mpsc::Sender<Vec<AvailabilityChange>>) -> Self {
        Self(sender)
    }
}

#[async_trait]
impl AvailabilityChangeSink for ChannelSink {
    async fn publish(&self, changes: &[AvailabilityChange]) -> Result<()> {
        self.0
            .send(changes.to_vec())
            .await
            .context("availability-change receiver dropped")
    }
}
