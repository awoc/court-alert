use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use hmac::{Hmac, KeyInit, Mac};
use serde::Serialize;
use sha2::Sha256;
use uuid::Uuid;

use super::slot::Slot;
use crate::model::BookableSlotId;
use crate::subscriptions::contract::{AlertSender, AvailabilityAlert};

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) struct WebhookAlertSender {
    client: reqwest::Client,
    url: reqwest::Url,
    secret: String,
}

impl WebhookAlertSender {
    pub(super) fn new(url: reqwest::Url, secret: String) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .context("building the webhook alert HTTP client")?;
        Ok(Self {
            client,
            url,
            secret,
        })
    }

    fn sign(&self, timestamp: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.secret.as_bytes())
            .expect("HMAC accepts keys of any length");
        mac.update(timestamp.as_bytes());
        mac.update(b".");
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }
}

#[derive(Serialize)]
struct Alert<'a> {
    user_id: &'a str,
    slots: Vec<Slot<'a>>,
}

#[async_trait]
impl AlertSender for WebhookAlertSender {
    async fn send_alert(&self, user_id: &str, alert: &AvailabilityAlert) -> Result<()> {
        let body = serde_json::to_vec(&Alert {
            user_id,
            slots: alert.slots.iter().map(Slot::from).collect(),
        })
        .context("encoding the webhook alert")?;
        let timestamp = Utc::now().timestamp().to_string();
        let signature = self.sign(&timestamp, &body);
        self.client
            .post(self.url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("X-Webhook-Timestamp", timestamp)
            .header("X-Webhook-Signature-V2", signature)
            .header("webhook-id", Uuid::new_v4().to_string())
            .body(body)
            .send()
            .await
            .context("sending the webhook alert")?
            .error_for_status()
            .context("the webhook alert was rejected")?;
        Ok(())
    }

    async fn strike_taken(&self, _slots: &[BookableSlotId]) -> Result<()> {
        Ok(())
    }
}
