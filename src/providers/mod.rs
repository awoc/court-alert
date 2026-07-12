pub mod discord;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::watch;

use crate::config::Settings;
use crate::domain::ProviderUserRef;
use crate::subscriptions::SubscriptionService;

#[async_trait]
pub trait ChatProvider: Send {
    fn admins(&self) -> HashSet<ProviderUserRef>;

    async fn run(
        self: Box<Self>,
        service: Arc<SubscriptionService>,
        ready: ReadySignal,
    ) -> Result<()>;
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

pub fn build(settings: &Settings) -> Vec<Box<dyn ChatProvider>> {
    let mut providers: Vec<Box<dyn ChatProvider>> = Vec::new();
    if let Some(discord) = &settings.discord_bot {
        providers.push(Box::new(discord::DiscordProvider::new(discord.clone())));
    }
    providers
}

pub async fn run_all(
    providers: Vec<Box<dyn ChatProvider>>,
    service: Arc<SubscriptionService>,
    signals: Vec<ReadySignal>,
) -> Result<()> {
    let mut tasks = tokio::task::JoinSet::new();
    for (provider, ready) in providers.into_iter().zip(signals) {
        tasks.spawn(provider.run(service.clone(), ready));
    }
    while let Some(res) = tasks.join_next().await {
        res.context("chat provider task panicked")??;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqliteStore;
    use crate::subscriptions::{BerlinClock, SubscriptionService};
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

    struct InstantReady;

    #[async_trait]
    impl ChatProvider for InstantReady {
        fn admins(&self) -> HashSet<ProviderUserRef> {
            HashSet::new()
        }

        async fn run(
            self: Box<Self>,
            _service: Arc<SubscriptionService>,
            ready: ReadySignal,
        ) -> Result<()> {
            ready.ready();
            Ok(())
        }
    }

    #[tokio::test]
    async fn run_all_hands_each_provider_its_ready_signal() {
        let store = Arc::new(SqliteStore::open_in_memory().await.unwrap());
        let service = Arc::new(SubscriptionService::new(
            store.clone(),
            store,
            HashSet::new(),
            Vec::new(),
            Arc::new(BerlinClock),
        ));
        let (signals, barrier) = readiness(2);

        run_all(
            vec![Box::new(InstantReady), Box::new(InstantReady)],
            service,
            signals,
        )
        .await
        .unwrap();

        barrier.wait().await;
    }
}
