use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context as _, Result};
use tracing::{info, warn};

use crate::config::{Config, Settings};
use crate::model::{Provider, VenueIdentity, VenueRegistry};
use crate::monitor::Monitor;
use crate::notify::{ChannelSink, DiscordNotifier, SurfaceFilteredSink};
use crate::ports::{AvailabilityChangeSink, SlotAvailabilitySource};
use crate::providers::{self, ChatProvider};
use crate::store::SqliteStore;
use crate::subscriptions::{BerlinClock, SubscriptionService};
use crate::zhs::{Auth, ZhsSlotAvailabilitySource};

const PROVIDER_READY_TIMEOUT: Duration = Duration::from_secs(60);

pub struct App {
    config: Config,
    registry: Arc<RwLock<VenueRegistry>>,
    slot_source: Arc<dyn SlotAvailabilitySource>,
    sinks: Vec<Box<dyn AvailabilityChangeSink>>,
    chat_providers: Vec<Box<dyn ChatProvider>>,
    store: Arc<SqliteStore>,
}

impl App {
    pub async fn assemble(settings: Settings) -> Result<Self> {
        let config = Config::load(&settings.config_path)?;
        let registry = Arc::new(RwLock::new(build_registry(&config)));
        let chat_providers = providers::build(&settings);

        let store = Arc::new(SqliteStore::open(settings.db_path.clone()).await?);

        let mut sinks: Vec<Box<dyn AvailabilityChangeSink>> = Vec::new();
        match &settings.discord_webhook {
            Some(url) => sinks.push(SurfaceFilteredSink::wrap(
                Box::new(DiscordNotifier::new(url.clone(), store.clone())?),
                registry.clone(),
                config.surface_filter(),
            )),
            None => info!("COURT_ALERT_DISCORD_WEBHOOK not set — webhook notifier disabled"),
        }
        info!(
            venues = config.venues().len(),
            courts = config
                .venues()
                .iter()
                .filter_map(|venue| config.catalog_for(&venue.id))
                .map(|catalog| catalog.courts().len())
                .sum::<usize>(),
            poll_interval_secs = config.poll_interval_secs(),
            lookahead_days = config.lookahead_days(),
            surface_filter = %config.surface_filter(),
            webhook_notifiers = sinks.len(),
            chat_providers = chat_providers.len(),
            admins = chat_providers
                .iter()
                .map(|p| p.admins().len())
                .sum::<usize>(),
            config_path = %settings.config_path.display(),
            db_path = %settings.db_path.display(),
            "starting court-alert"
        );

        let slot_source: Arc<dyn SlotAvailabilitySource> = Arc::new(
            ZhsSlotAvailabilitySource::new(zhs_auth(&config, settings.credentials)?),
        );

        Ok(Self {
            config,
            registry,
            slot_source,
            sinks,
            chat_providers,
            store,
        })
    }

    pub async fn run(self) -> Result<()> {
        if self.chat_providers.is_empty() {
            Monitor::new(
                self.config,
                self.registry,
                self.slot_source,
                self.sinks,
                self.store,
            )
            .run()
            .await
        } else {
            self.run_with_providers().await
        }
    }

    async fn run_with_providers(self) -> Result<()> {
        let Self {
            config,
            registry,
            slot_source,
            mut sinks,
            chat_providers,
            store,
        } = self;

        let (tx, rx) = tokio::sync::mpsc::channel(2);
        sinks.push(Box::new(ChannelSink::new(tx)));

        let admins = chat_providers.iter().flat_map(|p| p.admins()).collect();
        let service = Arc::new(SubscriptionService::new(
            store.clone(),
            store.clone(),
            admins,
            registry.clone(),
            config.surface_filter(),
            Arc::new(BerlinClock),
        ));
        let dispatch = tokio::spawn(service.clone().dispatch_loop(rx));

        let (ready_signals, ready_barrier) = providers::readiness(chat_providers.len());

        tokio::try_join!(
            async {
                match tokio::time::timeout(PROVIDER_READY_TIMEOUT, ready_barrier.wait()).await {
                    Ok(()) => info!("chat providers ready; starting monitor"),
                    Err(_) => warn!(
                        timeout_secs = PROVIDER_READY_TIMEOUT.as_secs(),
                        "chat providers not ready; starting monitor anyway"
                    ),
                }
                Monitor::new(config, registry, slot_source, sinks, store)
                    .run()
                    .await
            },
            providers::run_all(chat_providers, service, ready_signals),
            async {
                dispatch.await.context("dispatch loop task failed")?;
                Ok(())
            }
        )?;
        Ok(())
    }
}

/// Seeds the registry from config: ZHS venues arrive with their catalog, and
/// every other venue starts `Unresolved` for its own loop to fill in.
fn build_registry(config: &Config) -> VenueRegistry {
    let mut registry = VenueRegistry::new();
    for venue in config.venues() {
        registry.register(venue.id.clone(), venue.sport);
        if let Some(catalog) = config.catalog_for(&venue.id) {
            registry.set_catalog(venue.id.clone(), catalog.clone());
        }
    }
    registry
}

/// One set of ZHS credentials covers one ZHS deployment, which is all there is.
fn zhs_auth(config: &Config, credentials: crate::config::Credentials) -> Result<Auth> {
    let base_url = config
        .venues()
        .iter()
        .find_map(|venue| match &venue.identity {
            VenueIdentity::Zhs { base_url } => Some(base_url.clone()),
            _ => None,
        })
        .context("no ZHS venue configured")?;

    let extra = config
        .venues()
        .iter()
        .filter(|venue| venue.provider() == Provider::Zhs)
        .count();
    anyhow::ensure!(
        extra == 1,
        "only one ZHS venue is supported (found {extra}); \
         a second would need its own credentials"
    );

    Auth::new(base_url, credentials)
}
