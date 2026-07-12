use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{Timelike, Utc};
use chrono_tz::Europe::Berlin;
use futures::{StreamExt, TryStreamExt, stream};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::domain::{AvailabilityChange, BookableSlotSnapshot, diff_availability};
use crate::ports::{
    AvailabilityChangeSink, BookableSlotSnapshotRepository, SlotAvailabilitySource,
};
use crate::time::utc_day_window;

use self::logging::log_changes;
use self::snapshot::build_snapshot;

mod logging;
mod snapshot;
#[cfg(test)]
mod tests;

const MAX_CONCURRENT_COURT_FETCHES: usize = 4;

pub struct Monitor {
    config: Config,
    source: Arc<dyn SlotAvailabilitySource>,
    sinks: Vec<Box<dyn AvailabilityChangeSink>>,
    snapshots: Arc<dyn BookableSlotSnapshotRepository>,
}

impl Monitor {
    pub fn new(
        config: Config,
        source: Arc<dyn SlotAvailabilitySource>,
        sinks: Vec<Box<dyn AvailabilityChangeSink>>,
        snapshots: Arc<dyn BookableSlotSnapshotRepository>,
    ) -> Self {
        Self {
            config,
            source,
            sinks,
            snapshots,
        }
    }

    pub async fn run(self) -> Result<()> {
        let previous = self
            .snapshots
            .load_snapshot()
            .await
            .context("loading the bookable-slot snapshot")?;
        let mut state = MonitorState::new(previous, self.config.quiet_first_poll());
        let mut interval = self.polling_interval();

        loop {
            interval.tick().await;
            if let Err(error) = self.tick(&mut state).await {
                warn!(
                    error = %format!("{error:#}"),
                    "monitor tick failed; retaining previous snapshot"
                );
            }
        }
    }

    fn polling_interval(&self) -> tokio::time::Interval {
        let mut interval =
            tokio::time::interval(Duration::from_secs(self.config.poll_interval_secs()));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval
    }

    async fn tick(&self, state: &mut MonitorState) -> Result<()> {
        if !is_within_operating_window(
            Utc::now(),
            self.config.operating_window_start_hour(),
            self.config.operating_window_end_hour(),
        ) {
            debug!(
                start_hour = self.config.operating_window_start_hour(),
                end_hour = self.config.operating_window_end_hour(),
                "outside Berlin operating window; skipping monitor tick"
            );
            return Ok(());
        }

        let current = self.fetch_snapshot().await?;
        let changes = diff_availability(&state.previous, &current);

        self.persist_if_changed(&state.previous, &current).await?;
        log_changes(&changes, current.len());

        if state.suppress_next_publish {
            info!(
                suppressed = changes.len(),
                "first poll on empty slot snapshot; notifications suppressed"
            );
        } else if !changes.is_empty() {
            self.publish(&changes).await;
        }

        state.commit(current);
        Ok(())
    }

    async fn persist_if_changed(
        &self,
        previous: &BookableSlotSnapshot,
        current: &BookableSlotSnapshot,
    ) -> Result<()> {
        if current == previous {
            return Ok(());
        }

        self.snapshots
            .replace_snapshot(current.values().cloned().collect())
            .await
            .context("persisting the bookable-slot snapshot")
    }

    async fn publish(&self, changes: &[AvailabilityChange]) {
        for sink in &self.sinks {
            if let Err(error) = sink.publish(changes).await {
                warn!(
                    error = %format!("{error:#}"),
                    "availability-change sink failed"
                );
            }
        }
    }

    async fn fetch_snapshot(&self) -> Result<BookableSlotSnapshot> {
        let (starts_at, ends_at) = utc_day_window(self.config.lookahead_days());
        debug!(
            window_start = %starts_at,
            window_end = %ends_at,
            courts = self.config.courts().len(),
            "querying slot availability"
        );

        let source = self.source.as_ref();
        let fetched = stream::iter(self.config.courts())
            .map(|court| async move {
                let observations =
                    source
                        .fetch(court, starts_at, ends_at)
                        .await
                        .with_context(|| {
                            format!(
                                "fetching availability for court {} ({})",
                                court.name(),
                                court.id()
                            )
                        })?;
                Ok::<_, anyhow::Error>((court, observations))
            })
            .buffer_unordered(MAX_CONCURRENT_COURT_FETCHES)
            .try_collect::<Vec<_>>()
            .await?;

        Ok(build_snapshot(fetched, Utc::now()))
    }
}

fn is_within_operating_window(now: chrono::DateTime<Utc>, start_hour: u32, end_hour: u32) -> bool {
    (start_hour..end_hour).contains(&now.with_timezone(&Berlin).hour())
}

struct MonitorState {
    previous: BookableSlotSnapshot,
    suppress_next_publish: bool,
}

impl MonitorState {
    fn new(previous: BookableSlotSnapshot, quiet_first_poll: bool) -> Self {
        let suppress_next_publish = quiet_first_poll && previous.is_empty();
        Self {
            previous,
            suppress_next_publish,
        }
    }

    fn commit(&mut self, current: BookableSlotSnapshot) {
        self.previous = current;
        self.suppress_next_publish = false;
    }
}
