mod format;
mod webhook;

pub use webhook::DiscordNotifier;

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::domain::AvailabilityChange;
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
