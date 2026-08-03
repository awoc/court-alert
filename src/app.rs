use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context as _, Result};
use tracing::{info, warn};

use crate::chat;
use crate::config::{Config, Settings};
use crate::model::{Sport, VenueRegistry};
use crate::monitor::Monitor;
use crate::notify::{ChannelSink, DiscordNotifier, SportScopedSink, SurfaceFilteredSink};
use crate::ports::{AvailabilityChangeSink, ProviderSources};
use crate::sources;
use crate::store::SqliteStore;
use crate::subscriptions::{BerlinClock, SubscriptionService};

const PROVIDER_READY_TIMEOUT: Duration = Duration::from_secs(60);

pub struct App {
    config: Config,
    registry: Arc<RwLock<VenueRegistry>>,
    sources: ProviderSources,
    sinks: Vec<Arc<dyn AvailabilityChangeSink>>,
    chat_providers: Vec<Box<dyn chat::ChatProvider>>,
    store: Arc<SqliteStore>,
}

impl App {
    pub async fn assemble(settings: Settings) -> Result<Self> {
        let config = Config::load(&settings.config_path)?;
        // Each adapter states its own limits; the composition root is simply
        // where both are consulted.
        let describe = || format!("validating config file {}", settings.config_path.display());
        sources::validate_configuration(&config).with_context(describe)?;
        chat::validate_configuration(&config).with_context(describe)?;
        let registry = Arc::new(RwLock::new(build_registry(&config)));
        let chat_providers = chat::build(&settings);

        let store = Arc::new(SqliteStore::open(settings.db_path.clone()).await?);

        let mut sinks: Vec<Arc<dyn AvailabilityChangeSink>> = Vec::new();
        match &settings.discord_webhook {
            // The sport scope is the outer wrapper and is never a no-op: padel
            // is DM-only, and the surface filter would let it through whenever
            // `surface_filter = "all"` removes that filter from the chain.
            Some(url) => sinks.push(
                SportScopedSink::wrap(
                    SurfaceFilteredSink::wrap(
                        Box::new(DiscordNotifier::new(url.clone(), store.clone())?),
                        registry.clone(),
                        config.surface_filter(),
                    ),
                    registry.clone(),
                    Sport::Tennis,
                )
                .into(),
            ),
            None => info!("COURT_ALERT_DISCORD_WEBHOOK not set — webhook notifier disabled"),
        }
        info!(
            venues = config.venues().len(),
            // Only the ones config declares; Playtomic venues discover theirs
            // on their first poll and would otherwise read as zero courts.
            configured_courts = config
                .venues()
                .iter()
                .filter_map(|venue| config.catalog_for(&venue.id))
                .map(|catalog| catalog.courts().len())
                .sum::<usize>(),
            discovering_venues = config
                .venues()
                .iter()
                .filter(|venue| config.catalog_for(&venue.id).is_none())
                .count(),
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

        let sources = sources::build(&config, settings.credentials)?;

        Ok(Self {
            config,
            registry,
            sources,
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
                self.sources,
                self.sinks,
                self.store.clone(),
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
            sources,
            mut sinks,
            chat_providers,
            store,
        } = self;

        let (tx, rx) = tokio::sync::mpsc::channel(2);
        sinks.push(Arc::new(ChannelSink::new(tx)));

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

        let (ready_signals, ready_barrier) = chat::readiness(chat_providers.len());

        tokio::try_join!(
            async {
                match tokio::time::timeout(PROVIDER_READY_TIMEOUT, ready_barrier.wait()).await {
                    Ok(()) => info!("chat providers ready; starting monitor"),
                    Err(_) => warn!(
                        timeout_secs = PROVIDER_READY_TIMEOUT.as_secs(),
                        "chat providers not ready; starting monitor anyway"
                    ),
                }
                Monitor::new(config, registry, sources, sinks, store.clone(), store)
                    .run()
                    .await
            },
            chat::run_all(chat_providers, service, ready_signals),
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
        registry.register(venue);
        if let Some(catalog) = config.catalog_for(&venue.id) {
            registry.set_catalog(&venue.id, catalog.clone());
        }
    }
    registry
}
