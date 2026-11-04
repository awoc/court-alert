pub mod discord;
pub mod webhook;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::watch;

use crate::alerts::AlertMessageLifecycle;
use crate::config::Settings;
use crate::model::{IntegrationId, SubscriberRef};
use crate::subscriptions::{AlertRoute, ClubDirectory, SubscriptionService};

#[derive(Clone)]
pub struct IntegrationServices {
    pub subscriptions: Arc<SubscriptionService>,
    pub clubs: ClubDirectory,
}

#[async_trait]
pub trait Integration: Send {
    fn admins(&self) -> HashSet<SubscriberRef> {
        HashSet::new()
    }

    fn alert_route(&self) -> Result<(IntegrationId, AlertRoute)>;

    async fn run(self: Box<Self>, services: IntegrationServices, ready: ReadySignal) -> Result<()>;
}

pub struct ReadySignal {
    fired: AtomicBool,
    tx: watch::Sender<usize>,
}

impl ReadySignal {
    pub fn ready(&self) {
        if !self.fired.swap(true, Ordering::SeqCst) {
            self.tx.send_modify(|count| *count += 1);
        }
    }
}

pub struct ReadyBarrier {
    expected: usize,
    rx: watch::Receiver<usize>,
}

impl ReadyBarrier {
    pub async fn wait(mut self) {
        let all_ready = self.rx.wait_for(|count| *count >= self.expected).await;
        if all_ready.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

pub fn readiness(n: usize) -> (Vec<ReadySignal>, ReadyBarrier) {
    let (tx, rx) = watch::channel(0);
    let signals = (0..n)
        .map(|_| ReadySignal {
            fired: AtomicBool::new(false),
            tx: tx.clone(),
        })
        .collect();
    (signals, ReadyBarrier { expected: n, rx })
}

pub fn build(
    settings: &Settings,
    alert_lifecycle: Arc<AlertMessageLifecycle>,
) -> Vec<Box<dyn Integration>> {
    let mut integrations: Vec<Box<dyn Integration>> = Vec::new();
    if let Some(discord) = &settings.discord_bot {
        integrations.push(Box::new(discord::DiscordIntegration::new(
            discord.clone(),
            alert_lifecycle,
        )));
    }
    if let Some(webhook) = &settings.webhook {
        integrations.push(Box::new(webhook::WebhookIntegration::new(webhook.clone())));
    }
    integrations
}

pub fn alert_routes(
    integrations: &[Box<dyn Integration>],
) -> Result<HashMap<IntegrationId, AlertRoute>> {
    integrations
        .iter()
        .map(|integration| integration.alert_route())
        .collect()
}

pub async fn run_all(
    integrations: Vec<Box<dyn Integration>>,
    services: IntegrationServices,
    signals: Vec<ReadySignal>,
) -> Result<()> {
    let mut tasks = tokio::task::JoinSet::new();
    for (integration, ready) in integrations.into_iter().zip(signals) {
        tasks.spawn(integration.run(services.clone(), ready));
    }
    while let Some(res) = tasks.join_next().await {
        res.context("integration task panicked")??;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::store::SqliteStore;
    use crate::subscriptions::BerlinClock;
    use crate::subscriptions::contract::{AlertSender, AvailabilityAlert};
    use std::time::Duration;

    #[tokio::test]
    async fn barrier_resolves_once_all_signals_fire() {
        let (signals, barrier) = readiness(2);
        for signal in &signals {
            signal.ready();
        }
        barrier.wait().await;
    }

    #[tokio::test(start_paused = true)]
    async fn double_fire_from_one_signal_counts_once() {
        let (signals, barrier) = readiness(2);
        signals[0].ready();
        signals[0].ready();
        assert!(
            tokio::time::timeout(Duration::from_secs(60), barrier.wait())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn empty_barrier_resolves_immediately() {
        let (signals, barrier) = readiness(0);
        assert!(signals.is_empty());
        barrier.wait().await;
    }

    #[tokio::test(start_paused = true)]
    async fn dropped_unfired_signal_leaves_barrier_pending() {
        let (signals, barrier) = readiness(1);
        drop(signals);
        assert!(
            tokio::time::timeout(Duration::from_secs(60), barrier.wait())
                .await
                .is_err()
        );
    }

    struct NullSender;

    #[async_trait]
    impl AlertSender for NullSender {
        async fn send_alert(&self, _: &str, _: &AvailabilityAlert) -> Result<()> {
            Ok(())
        }
    }

    struct InstantReady;

    #[async_trait]
    impl Integration for InstantReady {
        fn alert_route(&self) -> Result<(IntegrationId, AlertRoute)> {
            Ok((
                "instant".into(),
                AlertRoute::delivery_only(Arc::new(NullSender)),
            ))
        }

        async fn run(
            self: Box<Self>,
            _services: IntegrationServices,
            ready: ReadySignal,
        ) -> Result<()> {
            ready.ready();
            Ok(())
        }
    }

    #[tokio::test]
    async fn run_all_hands_each_integration_its_ready_signal() {
        let store = Arc::new(SqliteStore::open_in_memory().await.unwrap());
        let registry = Arc::new(std::sync::RwLock::new(crate::model::VenueRegistry::new()));
        let services = IntegrationServices {
            subscriptions: Arc::new(SubscriptionService::new(
                store.clone(),
                store.clone(),
                HashSet::new(),
                registry.clone(),
                crate::model::CourtFilter::Any,
                Arc::new(BerlinClock),
            )),
            clubs: ClubDirectory::new(registry.clone()),
        };
        let (signals, barrier) = readiness(2);

        run_all(
            vec![Box::new(InstantReady), Box::new(InstantReady)],
            services,
            signals,
        )
        .await
        .unwrap();

        barrier.wait().await;
    }
    struct NoRoute;

    #[async_trait]
    impl Integration for NoRoute {
        fn alert_route(&self) -> Result<(IntegrationId, AlertRoute)> {
            anyhow::bail!("no route today")
        }

        async fn run(
            self: Box<Self>,
            _services: IntegrationServices,
            _ready: ReadySignal,
        ) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn each_integration_contributes_its_route() {
        let routes = alert_routes(&[Box::new(InstantReady)]).unwrap();

        assert_eq!(routes.len(), 1);
        assert!(routes.contains_key(&IntegrationId::from("instant")));
    }

    #[test]
    fn a_route_that_cannot_be_built_fails_the_whole_assembly() {
        let integrations: [Box<dyn Integration>; 2] = [Box::new(InstantReady), Box::new(NoRoute)];

        let error = alert_routes(&integrations).err().expect("a missing route");

        assert_eq!(error.to_string(), "no route today");
    }
}
