mod alerts;
mod api;
mod auth;
mod clubs;
mod response;
mod signing;
mod slot;
mod subscriptions;
#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::net::TcpListener;
use tracing::info;

use crate::config::WebhookSettings;
use crate::integrations::{Integration, IntegrationServices, ReadySignal};
use crate::model::IntegrationId;
use crate::subscriptions::{AlertRoute, ClubDirectory, SubscriptionService};
use alerts::WebhookAlertSender;

pub(super) const INTEGRATION: IntegrationId = IntegrationId::from_static("webhook");

pub struct WebhookIntegration {
    listen: SocketAddr,
    api_secret: Arc<str>,
    alert_url: reqwest::Url,
    alert_secret: String,
}

impl WebhookIntegration {
    pub fn new(settings: WebhookSettings) -> Self {
        Self {
            listen: settings.listen,
            api_secret: settings.api_secret.into(),
            alert_url: settings.alert_url,
            alert_secret: settings.alert_secret,
        }
    }

    pub async fn bind(self) -> Result<BoundWebhookApi> {
        let listener = TcpListener::bind(self.listen)
            .await
            .with_context(|| format!("binding the webhook API to {}", self.listen))?;
        Ok(BoundWebhookApi {
            listener,
            api_secret: self.api_secret,
        })
    }
}

pub struct BoundWebhookApi {
    listener: TcpListener,
    api_secret: Arc<str>,
}

impl BoundWebhookApi {
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.listener
            .local_addr()
            .context("reading the webhook API address")
    }

    pub async fn serve(
        self,
        service: Arc<SubscriptionService>,
        clubs: ClubDirectory,
    ) -> Result<()> {
        axum::serve(self.listener, api::router(service, clubs, self.api_secret))
            .await
            .context("serving the webhook API")
    }
}

#[async_trait]
impl Integration for WebhookIntegration {
    fn alert_route(&self) -> Result<(IntegrationId, AlertRoute)> {
        let alerts = WebhookAlertSender::new(self.alert_url.clone(), self.alert_secret.clone())?;
        Ok((INTEGRATION, AlertRoute::delivery_only(Arc::new(alerts))))
    }

    async fn run(self: Box<Self>, services: IntegrationServices, ready: ReadySignal) -> Result<()> {
        let webhook = self.bind().await?;
        info!(address = %webhook.local_addr()?, "webhook API listening");
        ready.ready();
        webhook.serve(services.subscriptions, services.clubs).await
    }
}
