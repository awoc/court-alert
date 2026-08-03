use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context as _, Result};
use tracing::{info, warn};

use crate::catalog::ConfiguredCatalogSource;
use crate::config::{Config, Credentials, Settings};
use crate::model::{Provider, Sport, VenueIdentity, VenueRegistry};
use crate::monitor::{Monitor, ProviderSources};
use crate::notify::{ChannelSink, DiscordNotifier, SportScopedSink, SurfaceFilteredSink};
use crate::playtomic::{
    ClubDirectory, PlaytomicAvailabilitySource, PlaytomicCatalogSource, PlaytomicClient,
};
use crate::ports::AvailabilityChangeSink;
use crate::providers::{self, ChatProvider};
use crate::store::SqliteStore;
use crate::subscriptions::{BerlinClock, SubscriptionService};
use crate::zhs::{Auth, ZhsSlotAvailabilitySource};

const PROVIDER_READY_TIMEOUT: Duration = Duration::from_secs(60);

pub struct App {
    config: Config,
    registry: Arc<RwLock<VenueRegistry>>,
    sources: ProviderSources,
    sinks: Vec<Arc<dyn AvailabilityChangeSink>>,
    chat_providers: Vec<Box<dyn ChatProvider>>,
    store: Arc<SqliteStore>,
}

impl App {
    pub async fn assemble(settings: Settings) -> Result<Self> {
        let config = Config::load(&settings.config_path)?;
        let registry = Arc::new(RwLock::new(build_registry(&config)));
        let chat_providers = providers::build(&settings);

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

        let sources = build_sources(&config, settings.credentials)?;

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
                Monitor::new(config, registry, sources, sinks, store.clone(), store)
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
        registry.register(venue);
        if let Some(catalog) = config.catalog_for(&venue.id) {
            registry.set_catalog(venue.id.clone(), catalog.clone());
        }
    }
    registry
}

/// Wires one availability adapter and one catalog source per provider in use.
///
/// Playtomic's two adapters share a client, so every club reuses the same
/// connection pool, and a directory, so the availability fetch can see the
/// opening hours discovery read off the club page.
fn build_sources(config: &Config, credentials: Option<Credentials>) -> Result<ProviderSources> {
    let mut sources = ProviderSources::new();
    let configured_catalogs = Arc::new(ConfiguredCatalogSource::new(config.catalogs().clone()));

    if config.uses(Provider::Zhs) {
        sources.insert(
            Provider::Zhs,
            Arc::new(ZhsSlotAvailabilitySource::new(zhs_auth(
                config,
                credentials,
            )?)),
            configured_catalogs.clone(),
        );
    }

    if config.uses(Provider::Playtomic) {
        let client = PlaytomicClient::new()?;
        let directory = ClubDirectory::new();
        sources.insert(
            Provider::Playtomic,
            Arc::new(PlaytomicAvailabilitySource::new(
                client.clone(),
                directory.clone(),
            )),
            Arc::new(PlaytomicCatalogSource::new(client, directory)),
        );
    }

    Ok(sources)
}

/// One set of ZHS credentials covers one ZHS deployment, which is all there is.
///
/// The credentials are demanded here rather than at startup, so a deployment
/// with only Playtomic venues needs none.
fn zhs_auth(config: &Config, credentials: Option<Credentials>) -> Result<Auth> {
    let credentials = credentials.context(
        "a ZHS venue is configured, so COURT_ALERT_EMAIL and COURT_ALERT_PASSWORD must be set",
    )?;
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

    const PADEL: &str = r#"
poll_interval_secs = 300
lookahead_days = 15

[[venues]]
id = "casa-padel"
display_name = "Casa Padel"
sport = "padel"
provider = "playtomic"
tenant_id = "f8483f72-1d14-49eb-a98b-e4b89d969c78"
slug = "casa-padel"
"#;

    /// Playtomic needs no credentials, as the README says, so a deployment with
    /// only Playtomic venues must start without the ZHS environment variables.
    #[test]
    fn a_playtomic_only_deployment_needs_no_credentials() {
        let config = Config::parse(PADEL).expect("parse");
        assert!(build_sources(&config, None).is_ok());
    }

    /// …but a ZHS venue still demands them, and says which ones.
    #[test]
    fn a_zhs_venue_without_credentials_names_the_missing_variables() {
        let config = Config::parse(ZHS).expect("parse");

        // Not `expect_err`: `ProviderSources` holds trait objects and has no
        // `Debug`, so the success arm cannot be formatted.
        let error = match build_sources(&config, None) {
            Ok(_) => panic!("missing credentials must be rejected"),
            Err(error) => error,
        };

        let message = format!("{error:#}");
        assert!(message.contains("COURT_ALERT_EMAIL"), "got: {message}");
        assert!(message.contains("COURT_ALERT_PASSWORD"), "got: {message}");
    }

    #[test]
    fn a_zhs_venue_with_credentials_wires_up() {
        let config = Config::parse(ZHS).expect("parse");
        let credentials = Credentials {
            email: "alice@example.com".into(),
            password: "hunter2".into(),
        };

        assert!(build_sources(&config, Some(credentials)).is_ok());
    }
}
