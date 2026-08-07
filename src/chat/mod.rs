pub mod discord;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::watch;

use crate::config::{Config, Settings};
use crate::model::{ProviderUserRef, Sport};
use crate::ports::AlertMessageRepository;
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

pub fn validate_configuration(config: &Config) -> Result<()> {
    let padel_venues = config
        .venues()
        .iter()
        .filter(|venue| venue.sport == Sport::Padel)
        .count();
    anyhow::ensure!(
        padel_venues <= discord::MAX_CLUB_CHOICES,
        "{padel_venues} padel venues are configured, but Discord allows at most {} \
         club choices on /padel; monitoring more would leave the rest selectable \
         only through \"all clubs\"",
        discord::MAX_CLUB_CHOICES
    );
    Ok(())
}

pub fn build(
    settings: &Settings,
    messages: Arc<dyn AlertMessageRepository>,
    pruner: Arc<discord::DailyPruner>,
) -> Vec<Box<dyn ChatProvider>> {
    let mut providers: Vec<Box<dyn ChatProvider>> = Vec::new();
    if let Some(discord) = &settings.discord_bot {
        providers.push(Box::new(discord::DiscordProvider::new(
            discord.clone(),
            messages,
            pruner,
        )));
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

    const ZHS: &str = r#"
poll_interval_secs = 300
lookahead_days = 7

[[venues]]
id = "zhs-munich"
display_name = "ZHS München"
sport = "tennis"
provider = "zhs"
base_url = "https://kurse.zhs-muenchen.de"

  [[venues.courts]]
  id = "92db7384-2dec-4888-a92a-4c2b6faac5f7"
  name = "Tennis Court 1"
"#;

    #[test]
    fn more_padel_venues_than_discord_can_offer_are_rejected() {
        let limit = discord::MAX_CLUB_CHOICES;
        let club = |i: usize| {
            format!(
                "\n[[venues]]\nid = \"club-{i}\"\ndisplay_name = \"Club {i}\"\n\
                 sport = \"padel\"\nprovider = \"playtomic\"\n\
                 tenant_id = \"f8483f72-1d14-49eb-a98b-e4b89d969c78\"\n\
                 slug = \"club-{i}\"\nlookahead_days = 15\n"
            )
        };
        let with = |count: usize| {
            let clubs: String = (0..count).map(club).collect();
            let config = Config::parse(&format!("{ZHS}{clubs}")).expect("parse");
            validate_configuration(&config)
        };

        assert!(with(limit).is_ok(), "{limit} clubs must still be accepted");

        let error = with(limit + 1).expect_err("more clubs than Discord can offer");
        assert!(
            format!("{error:#}").contains(&limit.to_string()),
            "the limit is not named: {error:#}"
        );
    }
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
            Arc::new(std::sync::RwLock::new(crate::model::VenueRegistry::new())),
            crate::model::CourtFilter::Any,
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
