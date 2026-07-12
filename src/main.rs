use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;

use court_alert::config::{Config, Settings};
use court_alert::monitor::Monitor;
use court_alert::notify::{ChannelSink, DiscordNotifier};
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
    init_tracing();

    let settings = Settings::from_env()?;
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

    if chat_providers.is_empty() {
        Monitor::new(config, slot_source, sinks, store).run().await
    } else {
        run_with_providers(config, slot_source, sinks, chat_providers, store).await
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_timer(BerlinTime)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
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
