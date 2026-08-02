use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{Timelike, Utc};
use chrono_tz::Europe::Berlin;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::model::{
    AvailabilityChange, BookableSlotSnapshot, OperatingWindow, Venue, VenueRegistry,
    diff_availability,
};
use crate::ports::{
    AvailabilityChangeSink, BookableSlotSnapshotRepository, VenueAvailabilitySource,
    VenueStateRepository,
};
use crate::time::utc_day_window;

use self::logging::log_changes;
use self::snapshot::build_snapshot;

mod logging;
mod snapshot;
#[cfg(test)]
mod tests;

/// Owns one poll loop per venue.
///
/// Independent loops rather than one global tick: staggering becomes free, a
/// venue that fails to fetch simply skips its own tick and touches nothing, and
/// per-venue intervals become possible.
pub struct Monitor {
    config: Config,
    registry: Arc<RwLock<VenueRegistry>>,
    source: Arc<dyn VenueAvailabilitySource>,
    sinks: Vec<Arc<dyn AvailabilityChangeSink>>,
    snapshots: Arc<dyn BookableSlotSnapshotRepository>,
    venue_state: Arc<dyn VenueStateRepository>,
}

impl Monitor {
    pub fn new(
        config: Config,
        registry: Arc<RwLock<VenueRegistry>>,
        source: Arc<dyn VenueAvailabilitySource>,
        sinks: Vec<Arc<dyn AvailabilityChangeSink>>,
        snapshots: Arc<dyn BookableSlotSnapshotRepository>,
        venue_state: Arc<dyn VenueStateRepository>,
    ) -> Self {
        Self {
            config,
            registry,
            source,
            sinks,
            snapshots,
            venue_state,
        }
    }

    pub async fn run(self) -> Result<()> {
        self.sweep_removed_venues().await;

        let count = self.config.venues().len();
        let mut loops = tokio::task::JoinSet::new();
        for (index, venue) in self.config.venues().iter().enumerate() {
            let interval = self.config.poll_interval_for(venue);
            loops.spawn(
                VenueLoop {
                    venue: venue.clone(),
                    interval: Duration::from_secs(interval),
                    lookahead_days: self.config.lookahead_days_for(venue),
                    operating_window: self.config.operating_window_for(venue),
                    quiet_first_poll: self.config.quiet_first_poll(),
                    registry: self.registry.clone(),
                    source: self.source.clone(),
                    sinks: self.sinks.clone(),
                    snapshots: self.snapshots.clone(),
                    venue_state: self.venue_state.clone(),
                }
                .run(phase_offset(index, count, interval)),
            );
        }

        while let Some(result) = loops.join_next().await {
            result.context("venue poll loop panicked")??;
        }
        Ok(())
    }

    /// Scoped replacement only ever touches a venue's own rows, so a club
    /// removed from config would keep its slots forever — and the subscribe
    /// preview would keep offering them.
    async fn sweep_removed_venues(&self) {
        let configured: Vec<_> = self
            .config
            .venues()
            .iter()
            .map(|venue| venue.id.clone())
            .collect();
        match self.snapshots.delete_snapshots_except(&configured).await {
            Ok(0) => {}
            Ok(removed) => info!(removed, "dropped slots of venues no longer configured"),
            Err(error) => warn!(
                error = %format!("{error:#}"),
                "sweeping slots of removed venues failed"
            ),
        }
    }
}

/// Spreads venue *i* of *n* evenly across the interval, so their bursts do not
/// land together. An optimisation, not a guarantee: a slow venue's retry can
/// still overlap the next one.
fn phase_offset(index: usize, count: usize, interval_secs: u64) -> Duration {
    if count <= 1 {
        return Duration::ZERO;
    }
    Duration::from_secs(interval_secs * index as u64 / count as u64)
}

struct VenueLoop {
    venue: Venue,
    interval: Duration,
    lookahead_days: i64,
    operating_window: OperatingWindow,
    quiet_first_poll: bool,
    registry: Arc<RwLock<VenueRegistry>>,
    source: Arc<dyn VenueAvailabilitySource>,
    sinks: Vec<Arc<dyn AvailabilityChangeSink>>,
    snapshots: Arc<dyn BookableSlotSnapshotRepository>,
    venue_state: Arc<dyn VenueStateRepository>,
}

impl VenueLoop {
    async fn run(self, offset: Duration) -> Result<()> {
        let previous = self
            .snapshots
            .load_venue_snapshot(&self.venue.id)
            .await
            .with_context(|| format!("loading the snapshot of venue {}", self.venue.id))?;
        let initialised = self
            .venue_state
            .is_initialised(&self.venue.id)
            .await
            .with_context(|| format!("reading the state of venue {}", self.venue.id))?;

        let mut state = MonitorState::new(previous, self.quiet_first_poll && !initialised);
        let mut failures = FailureRun::default();

        info!(
            venue = %self.venue.id,
            poll_interval_secs = self.interval.as_secs(),
            lookahead_days = self.lookahead_days,
            offset_secs = offset.as_secs(),
            "starting venue poll loop"
        );
        tokio::time::sleep(offset).await;

        let mut interval = tokio::time::interval(self.interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            match self.tick(&mut state).await {
                Ok(()) => failures.succeeded(&self.venue),
                Err(error) => failures.failed(&self.venue, &error),
            }
        }
    }

    async fn tick(&self, state: &mut MonitorState) -> Result<()> {
        if !is_within_operating_window(Utc::now(), self.operating_window) {
            debug!(
                venue = %self.venue.id,
                start_hour = self.operating_window.start_hour,
                end_hour = self.operating_window.end_hour,
                "outside Berlin operating window; skipping poll"
            );
            return Ok(());
        }

        // Cloned out of the registry so no lock is held across the fetch.
        let Some(catalog) = self
            .registry
            .read()
            .expect("venue registry poisoned")
            .catalog(&self.venue.id)
        else {
            debug!(venue = %self.venue.id, "court catalog not resolved yet; skipping poll");
            return Ok(());
        };

        let (starts_at, ends_at) = utc_day_window(self.lookahead_days);
        let observations = self
            .source
            .fetch(&self.venue, &catalog, starts_at, ends_at)
            .await
            .with_context(|| format!("fetching availability for venue {}", self.venue.id))?;
        let current = build_snapshot(&self.venue, observations, Utc::now());

        let changes = diff_availability(&state.previous, &current);
        self.persist_if_changed(&state.previous, &current).await?;
        log_changes(&self.venue, &changes, current.len());

        if state.suppress_next_publish {
            info!(
                venue = %self.venue.id,
                suppressed = changes.len(),
                "first poll of a new venue; notifications suppressed"
            );
        } else if !changes.is_empty() {
            self.publish(&changes).await;
        }

        // Recorded only after a successful poll, so a venue that never manages
        // one stays quiet rather than announcing its whole horizon later.
        self.venue_state
            .mark_initialised(&self.venue.id)
            .await
            .with_context(|| format!("recording the state of venue {}", self.venue.id))?;

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
            .replace_venue_snapshot(&self.venue.id, current.values().cloned().collect())
            .await
            .with_context(|| format!("persisting the snapshot of venue {}", self.venue.id))
    }

    async fn publish(&self, changes: &[AvailabilityChange]) {
        for sink in &self.sinks {
            if let Err(error) = sink.publish(changes).await {
                warn!(
                    venue = %self.venue.id,
                    error = %format!("{error:#}"),
                    "availability-change sink failed"
                );
            }
        }
    }
}

fn is_within_operating_window(now: chrono::DateTime<Utc>, window: OperatingWindow) -> bool {
    window.contains_hour(now.with_timezone(&Berlin).hour())
}

/// Reports the *transitions* into and out of failure rather than every tick
/// inside one.
///
/// `DiscordErrorLayer` forwards WARN events carrying an `error` field, so a
/// provider outage would otherwise be one message per venue per tick, forever.
#[derive(Default)]
struct FailureRun {
    failing: bool,
}

impl FailureRun {
    fn failed(&mut self, venue: &Venue, error: &anyhow::Error) {
        let message = format!("{error:#}");
        if self.failing {
            debug!(venue = %venue.id, reason = %message, "venue poll still failing");
            return;
        }
        self.failing = true;
        warn!(
            venue = %venue.id,
            error = %message,
            "venue poll failed; retaining its previous snapshot"
        );
    }

    fn succeeded(&mut self, venue: &Venue) {
        if std::mem::take(&mut self.failing) {
            info!(venue = %venue.id, "venue poll recovered");
        }
    }
}

struct MonitorState {
    previous: BookableSlotSnapshot,
    suppress_next_publish: bool,
}

impl MonitorState {
    /// Suppression keys on the venue never having polled, not on the slice
    /// being empty: a single club can legitimately be fully booked across its
    /// whole horizon, and inferring would swallow its next batch of free slots.
    fn new(previous: BookableSlotSnapshot, suppress_next_publish: bool) -> Self {
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
