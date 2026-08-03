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
use crate::providers::{self, ChatProvider, discord};
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
        validate_against_adapters(&config).with_context(|| {
            format!("validating config file {}", settings.config_path.display())
        })?;
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

/// The limits the adapters impose on a configuration.
///
/// These live at the composition root rather than in `Config::validate`,
/// because they are facts about the chosen adapters and not about the config
/// file: swap the chat provider and the club-choice limit is meaningless, yet
/// `Config` would go on enforcing it. Only this function knows both which
/// adapters are wired up and what they can do.
fn validate_against_adapters(config: &Config) -> Result<()> {
    for venue in config.venues() {
        // Beyond a provider's own horizon every extra day is a request that can
        // only come back empty.
        if let Some(cap) = provider_lookahead_cap(venue.provider()) {
            let requested = config.lookahead_days_for(venue);
            anyhow::ensure!(
                requested <= cap,
                "venue {}: {} serves at most {cap} days, but lookahead_days is {requested}",
                venue.id,
                venue.provider(),
            );
        }
    }

    // Beyond this, `/padel` could not offer every club by name.
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

fn provider_lookahead_cap(provider: Provider) -> Option<i64> {
    match provider {
        // ZHS publishes no horizon; the booking window closes per slot instead.
        Provider::Zhs => None,
        Provider::Playtomic => Some(crate::playtomic::MAX_LOOKAHEAD_DAYS),
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

    let mut zhs = config
        .venues()
        .iter()
        .filter_map(|venue| match &venue.identity {
            VenueIdentity::Zhs { base_url } => Some(base_url.clone()),
            _ => None,
        });
    // Exactly one, checked as one step: the caller only reaches this under
    // `uses(Provider::Zhs)`, so "none" is a wiring bug rather than a
    // configuration one, and a second venue would need its own credentials.
    let base_url = zhs.next().context("no ZHS venue configured")?;
    let extra = zhs.count();
    anyhow::ensure!(
        extra == 0,
        "only one ZHS venue is supported (found {}); \
         a second would need its own credentials",
        extra + 1
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

    /// By the number, not by prose: Playtomic serves today…today+14
    /// *inclusive*, and the window is half-open, so 15 covers exactly the
    /// horizon and 16 adds a guaranteed-empty request.
    #[test]
    fn a_playtomic_venue_may_look_ahead_fifteen_days_but_not_sixteen() {
        let with = |days: i64| {
            let config = Config::parse(
                &PADEL.replace("lookahead_days = 15", &format!("lookahead_days = {days}")),
            )
            .expect("parse");
            validate_against_adapters(&config)
        };

        assert!(with(15).is_ok(), "15 days is exactly the horizon");
        let error = with(16).expect_err("16 days must be rejected");
        assert!(
            format!("{error:#}").contains("15"),
            "the cap is not named: {error:#}"
        );
    }

    /// ZHS publishes no horizon, so only the global limit applies.
    #[test]
    fn a_zhs_venue_is_not_capped_at_the_playtomic_horizon() {
        let config = Config::parse(&ZHS.replace("lookahead_days = 7", "lookahead_days = 30"))
            .expect("parse");

        assert!(validate_against_adapters(&config).is_ok());
    }

    /// Discord caps a command option at 25 choices, so beyond that `/padel`
    /// could not offer every club by name — and quietly dropping the rest would
    /// leave them selectable only through "all clubs".
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
            validate_against_adapters(&config)
        };

        assert!(with(limit).is_ok(), "{limit} clubs must still be accepted");

        let error = with(limit + 1).expect_err("more clubs than Discord can offer");
        assert!(
            format!("{error:#}").contains(&limit.to_string()),
            "the limit is not named: {error:#}"
        );
    }

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
