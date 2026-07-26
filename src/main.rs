use std::time::Duration;

use anyhow::Result;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

use court_alert::app::App;
use court_alert::config::Settings;
use court_alert::notify::DiscordErrorLayer;
use court_alert::time::BerlinTime;

#[tokio::main]
async fn main() -> Result<()> {
    let settings = Settings::from_env()?;
    let monitoring_enabled = settings.discord_error_webhook.is_some();
    init_tracing(settings.discord_error_webhook.clone())?;

    let app = App::assemble(settings).await?;

    let result = app.run().await;
    if let Err(error) = &result {
        error!(error = %format!("{error:#}"), "court-alert stopped");
        if monitoring_enabled {
            // Give the asynchronous monitoring-webhook worker time to flush a fatal error.
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    result
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
