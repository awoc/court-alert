use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context as _, Result};
use tracing::{info, warn};

use crate::alerts::AlertMessageLifecycle;
use crate::config::{Config, Settings};
use crate::integrations::discord::DiscordNotifier;
use crate::integrations::{self, Integration, IntegrationServices};
use crate::model::{Sport, VenueRegistry};
use crate::monitor::Monitor;
use crate::notify::{ChannelSink, SportScopedSink, SurfaceFilteredSink};
use crate::ports::{AvailabilityChangeSink, ProviderSources};
use crate::sources;
use crate::store::SqliteStore;
use crate::subscriptions::{
    AlertDispatcher, BerlinClock, ClubDirectory, CourtNameConversion, SubscriptionService,
};

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
        if settings.discord_bot.is_some() {
            integrations::discord::validate_configuration(&config).with_context(describe)?;
        }
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

        let routes = integrations::alert_routes(&integrations)?;

        let (tx, rx) = tokio::sync::mpsc::channel(2);
        sinks.push(Arc::new(ChannelSink::new(tx)));

        let admins = integrations.iter().flat_map(|i| i.admins()).collect();
        let services = IntegrationServices {
            subscriptions: Arc::new(SubscriptionService::new(
                store.clone(),
                store.clone(),
                admins,
                registry.clone(),
                config.surface_filter(),
                Arc::new(BerlinClock),
            )),
            clubs: ClubDirectory::new(registry.clone()),
        };
        let dispatcher = AlertDispatcher::new(
            store.clone(),
            registry.clone(),
            routes,
            Arc::new(BerlinClock),
        );
        let dispatch = tokio::spawn(Arc::new(dispatcher).dispatch_loop(rx));
        tokio::spawn(
            CourtNameConversion::new(store.clone(), registry.clone(), Arc::new(BerlinClock))
                .run_until_done(),
        );

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
            integrations::run_all(integrations, services, ready_signals),
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use chrono::{DateTime, TimeZone, Utc};
    use tokio::sync::Notify;
    use tokio::time::Instant;

    use super::*;
    use crate::integrations::ReadySignal;
    use crate::model::{
        CourtCatalog, CourtFilter, CourtSelection, IntegrationId, Provider, Schedule,
        SlotObservation, SubscriberRef, TimeRange, UncheckedDraft, Venue,
    };
    use crate::ports::{CourtCatalogSource, SubscriptionRepository, VenueAvailabilitySource};
    use crate::subscriptions::AlertRoute;
    use crate::subscriptions::contract::{AlertSender, AvailabilityAlert};

    const CONFIG: &str = r#"
poll_interval_secs = 300
lookahead_days = 7
quiet_first_poll = false
operating_window_start_hour = 0
operating_window_end_hour = 24
surface_filter = "all"

[[venues]]
id = "zhs-munich"
display_name = "ZHS München"
sport = "tennis"
provider = "zhs"
base_url = "https://kurse.zhs-muenchen.de"

  [[venues.courts]]
  id = "00000000-0000-0000-0000-000000000001"
  name = "Tennis Court 1"
"#;

    #[derive(Default)]
    struct OneCourtFree {
        polls: AtomicUsize,
    }

    #[async_trait]
    impl VenueAvailabilitySource for OneCourtFree {
        async fn fetch(
            &self,
            venue: &Venue,
            catalog: &CourtCatalog,
            _starts_at: DateTime<Utc>,
            _ends_at: DateTime<Utc>,
        ) -> Result<Vec<SlotObservation>> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            let court = &catalog.courts()[0];
            let starts_at = Utc.with_ymd_and_hms(2026, 6, 2, 8, 0, 0).unwrap();
            Ok(vec![SlotObservation {
                venue_id: venue.id.clone(),
                court_id: court.id(),
                court_name: court.name().to_owned(),
                starts_at,
                ends_at: starts_at + chrono::Duration::hours(1),
                booking_closes_at: None,
                available_places: 1,
                already_booked: false,
                already_in_cart: false,
                already_on_waiting_list: false,
                blocked_by_resource: false,
            }])
        }
    }

    #[async_trait]
    impl CourtCatalogSource for OneCourtFree {
        async fn discover(&self, _venue: &Venue) -> Result<CourtCatalog> {
            anyhow::bail!("the test venue's courts are configured")
        }
    }

    #[derive(Default)]
    struct RecordingSender {
        subscribers: Mutex<Vec<String>>,
        delivered: Notify,
    }

    #[async_trait]
    impl AlertSender for RecordingSender {
        async fn send_alert(&self, subscriber_id: &str, _: &AvailabilityAlert) -> Result<()> {
            self.subscribers
                .lock()
                .unwrap()
                .push(subscriber_id.to_owned());
            self.delivered.notify_one();
            Ok(())
        }
    }

    struct NeverReady(Arc<RecordingSender>);

    #[async_trait]
    impl Integration for NeverReady {
        fn alert_route(&self) -> Result<(IntegrationId, AlertRoute)> {
            Ok((
                "never-ready".into(),
                AlertRoute::delivery_only(self.0.clone()),
            ))
        }

        async fn run(
            self: Box<Self>,
            _services: IntegrationServices,
            _ready: ReadySignal,
        ) -> Result<()> {
            std::future::pending().await
        }
    }

    struct NoRoute;

    #[async_trait]
    impl Integration for NoRoute {
        fn alert_route(&self) -> Result<(IntegrationId, AlertRoute)> {
            anyhow::bail!("building the alert route failed")
        }

        async fn run(
            self: Box<Self>,
            _services: IntegrationServices,
            _ready: ReadySignal,
        ) -> Result<()> {
            std::future::pending().await
        }
    }

    async fn app_with(integration: Box<dyn Integration>, source: Arc<OneCourtFree>) -> App {
        let config = Config::parse(CONFIG).unwrap();
        let mut sources = ProviderSources::new();
        sources.insert(Provider::Zhs, source.clone(), source);
        App {
            registry: Arc::new(RwLock::new(build_registry(&config))),
            config,
            sources,
            sinks: Vec::new(),
            integrations: vec![integration],
            store: Arc::new(SqliteStore::open_in_memory().await.unwrap()),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_monitor_starting_after_the_readiness_timeout_still_delivers() {
        let sender = Arc::new(RecordingSender::default());
        let app = app_with(
            Box::new(NeverReady(sender.clone())),
            Arc::new(OneCourtFree::default()),
        )
        .await;
        app.store
            .add(
                UncheckedDraft {
                    subscriber: SubscriberRef {
                        integration: "never-ready".into(),
                        subscriber_id: "alice".into(),
                    },
                    sport: Sport::Tennis,
                    venue: None,
                    schedule: Schedule::Weekday(chrono::Weekday::Tue),
                    time_range: TimeRange::new(0, 24 * 60).unwrap(),
                    courts: CourtSelection::Every,
                    filter: CourtFilter::Any,
                }
                .assume_valid(),
            )
            .await
            .unwrap();
        let started = Instant::now();

        tokio::select! {
            result = app.run() => panic!("the app stopped: {result:?}"),
            () = tokio::time::sleep(Duration::from_secs(24 * 60 * 60)) => {
                panic!("the alert was never delivered")
            }
            () = sender.delivered.notified() => {}
        }

        assert!(
            started.elapsed() >= INTEGRATION_READY_TIMEOUT,
            "the monitor did not wait for the readiness timeout"
        );
        assert_eq!(*sender.subscribers.lock().unwrap(), ["alice"]);
    }

    #[tokio::test]
    async fn a_route_that_cannot_be_built_ends_the_run_before_monitoring_starts() {
        let source = Arc::new(OneCourtFree::default());
        let app = app_with(Box::new(NoRoute), source.clone()).await;

        let error = tokio::time::timeout(Duration::from_secs(5), app.run())
            .await
            .expect("a failed route must end the run")
            .expect_err("a failed route is fatal");

        assert_eq!(error.to_string(), "building the alert route failed");
        assert_eq!(source.polls.load(Ordering::SeqCst), 0);
    }
}
