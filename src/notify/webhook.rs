use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Serialize;
use tracing::debug;

use crate::domain::AvailabilityChange;
use crate::ports::AvailabilityChangeSink;

use super::format::format_messages;

pub struct DiscordNotifier {
    webhook_url: reqwest::Url,
    client: reqwest::Client,
}

impl DiscordNotifier {
    pub fn new(webhook_url: reqwest::Url) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .context("building Discord HTTP client")?;
        Ok(Self {
            webhook_url,
            client,
        })
    }

    async fn send(&self, content: &str) -> Result<()> {
        #[derive(Serialize)]
        struct Body<'a> {
            content: &'a str,
        }
        let resp = self
            .client
            .post(self.webhook_url.clone())
            .json(&Body { content })
            .send()
            .await
            .map_err(reqwest::Error::without_url)
            .context("posting to Discord webhook")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!("Discord webhook returned {status}: {text}");
        }
        Ok(())
    }
}

#[async_trait]
impl AvailabilityChangeSink for DiscordNotifier {
    async fn publish(&self, changes: &[AvailabilityChange]) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        let messages = format_messages(changes);
        for (i, msg) in messages.iter().enumerate() {
            debug!(
                chunk = i + 1,
                of = messages.len(),
                bytes = msg.len(),
                "discord: sending"
            );
            self.send(msg).await?;
        }
        Ok(())
    }
}
