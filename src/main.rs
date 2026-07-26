use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

use court_alert::config::{Config, Settings};
use court_alert::monitor::Monitor;
use court_alert::notify::{ChannelSink, DiscordErrorLayer, DiscordNotifier};
use court_alert::ports::{AvailabilityChangeSink, SlotAvailabilitySource};
use court_alert::providers::{self, ChatProvider};
use court_alert::store::SqliteStore;
use court_alert::subscriptions::{BerlinClock, SubscriptionService};
use court_alert::zhs::{Auth, ZhsSlotAvailabilitySource};

const PROVIDER_READY_TIMEOUT: Duration = Duration::from_secs(60);

struct BerlinTime;

impl FormatTime for BerlinTime {
    fn format_time(&self, writer: &mut Writer<'_>) -> fmt::Result {
        let now = chrono::Utc::now().with_timezone(&chrono_tz::Europe::Berlin);
        write!(writer, "{}", now.format("%Y-%m-%d %H:%M:%S%.3f %Z"))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let settings = Settings::from_env()?;
    let monitoring_enabled = settings.discord_error_webhook.is_some();
    init_tracing(settings.discord_error_webhook.clone())?;

    let services = start(settings).await?;

    let result = run(services).await;
    if let Err(error) = &result {
        error!(error = %format!("{error:#}"), "court-alert stopped");
        if monitoring_enabled {
            // Give the asynchronous monitoring-webhook worker time to flush a fatal error.
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    result
}

struct Services {
    config: Config,
    slot_source: Arc<dyn SlotAvailabilitySource>,
    sinks: Vec<Box<dyn AvailabilityChangeSink>>,
    chat_providers: Vec<Box<dyn ChatProvider>>,
    store: Arc<SqliteStore>,
}

async fn start(settings: Settings) -> Result<Services> {
    let config = Config::load(&settings.config_path)?;
    let chat_providers = providers::build(&settings);

    let mut sinks: Vec<Box<dyn AvailabilityChangeSink>> = Vec::new();
    match &settings.discord_webhook {
        Some(url) => sinks.push(Box::new(DiscordNotifier::new(url.clone())?)),
        None => info!("COURT_ALERT_DISCORD_WEBHOOK not set — webhook notifier disabled"),
    }
    info!(
        courts = config.courts().len(),
        poll_interval_secs = config.poll_interval_secs(),
        lookahead_days = config.lookahead_days(),
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

    let auth = Auth::new(config.base_url().to_owned(), settings.credentials)?;
    let slot_source: Arc<dyn SlotAvailabilitySource> =
        Arc::new(ZhsSlotAvailabilitySource::new(auth));
    let store = Arc::new(SqliteStore::open(settings.db_path).await?);

    Ok(Services {
        config,
        slot_source,
        sinks,
        chat_providers,
        store,
    })
}

async fn run(services: Services) -> Result<()> {
    let Services {
        config,
        slot_source,
        sinks,
        chat_providers,
        store,
    } = services;

    if chat_providers.is_empty() {
        Monitor::new(config, slot_source, sinks, store).run().await
    } else {
        run_with_providers(config, slot_source, sinks, chat_providers, store).await
    }
}

fn init_tracing(error_webhook: Option<reqwest::Url>) -> Result<()> {
    let (error_layer, error_worker) = match error_webhook {
        Some(url) => {
            let (layer, worker) = DiscordErrorLayer::new(url)?;
            (Some(layer), Some(worker))
        }
        None => (None, None),
    };

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer().with_timer(BerlinTime))
        .with(error_layer)
        .init();

    if let Some(worker) = error_worker {
        tokio::spawn(worker.run());
    } else {
        info!("COURT_ALERT_DISCORD_ERROR_WEBHOOK not set — monitoring channel disabled");
    }
    Ok(())
}

async fn run_with_providers(
    config: Config,
    slot_source: Arc<dyn SlotAvailabilitySource>,
    mut sinks: Vec<Box<dyn AvailabilityChangeSink>>,
    chat_providers: Vec<Box<dyn ChatProvider>>,
    store: Arc<SqliteStore>,
) -> Result<()> {
    let (tx, rx) = tokio::sync::mpsc::channel(2);
    sinks.push(Box::new(ChannelSink::new(tx)));

    let admins = chat_providers.iter().flat_map(|p| p.admins()).collect();
    let service = Arc::new(SubscriptionService::new(
        store.clone(),
        store.clone(),
        admins,
        config.court_names(),
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
            Monitor::new(config, slot_source, sinks, store).run().await
        },
        providers::run_all(chat_providers, service, ready_signals),
        async {
            dispatch.await.context("dispatch loop task failed")?;
            Ok(())
        }
    )?;
    Ok(())
}
