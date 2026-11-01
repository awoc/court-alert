use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context as _, Result};
use tracing::{info, warn};

use crate::alerts::AlertMessageLifecycle;
use crate::config::{Config, Settings};
use crate::integrations::discord::DiscordNotifier;
use crate::integrations::{self, Integration};
use crate::model::{Sport, VenueRegistry};
use crate::monitor::Monitor;
use crate::notify::{ChannelSink, SportScopedSink, SurfaceFilteredSink};
use crate::ports::{AvailabilityChangeSink, ProviderSources};
use crate::sources;
use crate::store::SqliteStore;
use crate::subscriptions::{BerlinClock, SubscriptionService};

const INTEGRATION_READY_TIMEOUT: Duration = Duration::from_secs(60);

pub struct App {
    config: Config,
    registry: Arc<RwLock<VenueRegistry>>,
    sources: ProviderSources,
    sinks: Vec<Arc<dyn AvailabilityChangeSink>>,
    integrations: Vec<Box<dyn Integration>>,
    store: Arc<SqliteStore>,
}

impl App {
    pub async fn assemble(settings: Settings) -> Result<Self> {
        let config = Config::load(&settings.config_path)?;
        let describe = || format!("validating config file {}", settings.config_path.display());
        sources::validate_configuration(&config).with_context(describe)?;
        integrations::validate_configuration(&config, settings.discord_bot.as_ref())
            .with_context(describe)?;
        let registry = Arc::new(RwLock::new(build_registry(&config)));

        let store = Arc::new(SqliteStore::open(settings.db_path.clone()).await?);
        let alert_lifecycle = Arc::new(AlertMessageLifecycle::new(store.clone()));
        let integrations = integrations::build(&settings, alert_lifecycle.clone());

        let mut sinks: Vec<Arc<dyn AvailabilityChangeSink>> = Vec::new();
        match &settings.discord_webhook {
            Some(url) => sinks.push(
                SportScopedSink::wrap(
                    SurfaceFilteredSink::wrap(
                        Box::new(DiscordNotifier::new(url.clone(), alert_lifecycle.clone())?),
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
            integrations = integrations.len(),
            admins = integrations
                .iter()
                .map(|i| i.admins().len())
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
            integrations,
            store,
        })
    }

    pub async fn run(self) -> Result<()> {
        if self.integrations.is_empty() {
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
            self.run_with_integrations().await
        }
    }

    async fn run_with_integrations(self) -> Result<()> {
        let Self {
            config,
            registry,
            sources,
            mut sinks,
            integrations,
            store,
        } = self;

        let (tx, rx) = tokio::sync::mpsc::channel(2);
        sinks.push(Arc::new(ChannelSink::new(tx)));

        let admins = integrations.iter().flat_map(|i| i.admins()).collect();
        let service = Arc::new(SubscriptionService::new(
            store.clone(),
            store.clone(),
            admins,
            registry.clone(),
            config.surface_filter(),
            Arc::new(BerlinClock),
        ));
        let dispatch = tokio::spawn(service.clone().dispatch_loop(rx));

        let (ready_signals, ready_barrier) = integrations::readiness(integrations.len());

        tokio::try_join!(
            async {
                match tokio::time::timeout(INTEGRATION_READY_TIMEOUT, ready_barrier.wait()).await {
                    Ok(()) => info!("integrations ready; starting monitor"),
                    Err(_) => warn!(
                        timeout_secs = INTEGRATION_READY_TIMEOUT.as_secs(),
                        "integrations not ready; starting monitor anyway"
                    ),
                }
                Monitor::new(config, registry, sources, sinks, store.clone(), store)
                    .run()
                    .await
            },
            integrations::run_all(integrations, service, ready_signals),
            async {
                dispatch.await.context("dispatch loop task failed")?;
                Ok(())
            }
        )?;
        Ok(())
    }
}

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
