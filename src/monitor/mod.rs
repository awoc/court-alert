use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{Timelike, Utc};
use chrono_tz::Europe::Berlin;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::model::{
    AvailabilityChange, BookableSlotSnapshot, CourtCatalog, OperatingWindow, Venue, VenueRegistry,
    diff_availability,
};
use crate::ports::{
    AvailabilityChangeSink, BookableSlotSnapshotRepository, ProviderAdapters, ProviderSources,
    VenueStateRepository,
};
use crate::time::utc_day_window;

use self::report::report_changes;
use self::snapshot::build_snapshot;

mod report;
mod snapshot;
#[cfg(test)]
mod tests;

pub struct Monitor {
    config: Config,
    registry: Arc<RwLock<VenueRegistry>>,
    sources: ProviderSources,
    sinks: Vec<Arc<dyn AvailabilityChangeSink>>,
    snapshots: Arc<dyn BookableSlotSnapshotRepository>,
    venue_state: Arc<dyn VenueStateRepository>,
}

impl Monitor {
    pub fn new(
        config: Config,
        registry: Arc<RwLock<VenueRegistry>>,
        sources: ProviderSources,
        sinks: Vec<Arc<dyn AvailabilityChangeSink>>,
        snapshots: Arc<dyn BookableSlotSnapshotRepository>,
        venue_state: Arc<dyn VenueStateRepository>,
    ) -> Self {
        Self {
            config,
            registry,
            sources,
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
            let adapters = self
                .sources
                .get(venue.provider())
                .with_context(|| {
                    format!(
                        "venue {} needs a {} adapter, which was not wired up",
                        venue.id,
                        venue.provider()
                    )
                })?
                .clone();
            loops.spawn(
                VenueLoop {
                    venue: venue.clone(),
                    interval: Duration::from_secs(interval),
                    lookahead_days: self.config.lookahead_days_for(venue),
                    operating_window: self.config.operating_window_for(venue),
                    quiet_first_poll: self.config.quiet_first_poll(),
                    registry: self.registry.clone(),
                    adapters,
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
        match self
            .venue_state
            .delete_venue_state_except(&configured)
            .await
        {
            Ok(0) => {}
            Ok(removed) => info!(removed, "dropped state of venues no longer configured"),
            Err(error) => warn!(
                error = %format!("{error:#}"),
                "sweeping state of removed venues failed"
            ),
        }
    }
}

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
    adapters: ProviderAdapters,
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
        let mut catalog = DiscoveryState::default();
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
            match self.tick(&mut state, &mut catalog).await {
                Ok(TickOutcome::Polled) => failures.succeeded(&self.venue),
                Ok(TickOutcome::Skipped) => {}
                Err(error) => failures.failed(&self.venue, &error),
            }
        }
    }

    async fn tick(
        &self,
        state: &mut MonitorState,
        catalog: &mut DiscoveryState,
    ) -> Result<TickOutcome> {
        if !is_within_operating_window(Utc::now(), self.operating_window) {
            debug!(
                venue = %self.venue.id,
                start_hour = self.operating_window.start_hour,
                end_hour = self.operating_window.end_hour,
                "outside Berlin operating window; skipping poll"
            );
            return Ok(TickOutcome::Skipped);
        }

        let Some(courts) = self.resolve_catalog(catalog).await? else {
            return Ok(TickOutcome::Skipped);
        };

        let (starts_at, ends_at) = utc_day_window(self.lookahead_days);
        let observations = self
            .adapters
            .availability
            .fetch(&self.venue, &courts, starts_at, ends_at)
            .await
            .with_context(|| format!("fetching availability for venue {}", self.venue.id))?;

        if observations
            .iter()
            .any(|observation| courts.attributes_of(observation.court_id).is_none())
        {
            info!(
                venue = %self.venue.id,
                "availability names a court the catalog does not know; re-discovering"
            );
            catalog.invalidate();
        }

        let current = build_snapshot(&self.venue, observations, Utc::now());

        let changes = diff_availability(&state.previous, &current);
        self.persist_if_changed(&state.previous, &current).await?;
        report_changes(&self.venue, &changes, current.len());

        if state.suppress_next_publish {
            info!(
                venue = %self.venue.id,
                suppressed = changes.len(),
                "first poll of a new venue; notifications suppressed"
            );
        } else if !changes.is_empty() {
            self.publish(&changes).await;
        }

        // Commit before the marker write so its failure cannot republish changes.
        state.commit(current);

        if let Err(error) = self.venue_state.mark_initialised(&self.venue.id).await {
            warn!(
                venue = %self.venue.id,
                error = %format!("{error:#}"),
                "recording venue state failed; the next restart may re-suppress this venue"
            );
        }
        Ok(TickOutcome::Polled)
    }

    async fn resolve_catalog(
        &self,
        state: &mut DiscoveryState,
    ) -> Result<Option<Arc<CourtCatalog>>> {
        let cached = self
            .registry
            .read()
            .expect("venue registry poisoned")
            .catalog(&self.venue.id);
        if let Some(catalog) = &cached
            && !state.is_stale()
        {
            return Ok(Some(catalog.clone()));
        }
        if !state.may_attempt() {
            debug!(
                venue = %self.venue.id,
                "discovery is backing off; polling with the last known catalog"
            );
            return Ok(cached);
        }

        match self.adapters.catalogs.discover(&self.venue).await {
            Ok(discovered) => {
                info!(
                    venue = %self.venue.id,
                    courts = discovered.courts().len(),
                    "resolved court catalog"
                );
                let stored = self
                    .registry
                    .write()
                    .expect("venue registry poisoned")
                    .set_catalog(&self.venue.id, discovered);
                state.succeeded();
                stored
                    .with_context(|| format!("venue {} is not in the registry", self.venue.id))
                    .map(Some)
            }
            Err(error) => {
                state.failed();
                match cached {
                    Some(catalog) => {
                        warn!(
                            venue = %self.venue.id,
                            error = %format!("{error:#}"),
                            "refreshing the court catalog failed; keeping the last known one"
                        );
                        Ok(Some(catalog))
                    }
                    None => Err(error)
                        .with_context(|| format!("discovering courts of venue {}", self.venue.id)),
                }
            }
        }
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

#[derive(Debug, PartialEq, Eq)]
enum TickOutcome {
    Polled,
    Skipped,
}

const CATALOG_REFRESH: Duration = Duration::from_secs(24 * 60 * 60);

const MAX_DISCOVERY_BACKOFF_TICKS: u32 = 16;

#[derive(Default)]
struct DiscoveryState {
    resolved_at: Option<Instant>,
    consecutive_failures: u32,
    ticks_to_skip: u32,
}

impl DiscoveryState {
    fn is_stale(&self) -> bool {
        self.resolved_at
            .is_none_or(|at| at.elapsed() >= CATALOG_REFRESH)
    }

    fn may_attempt(&mut self) -> bool {
        if self.ticks_to_skip == 0 {
            return true;
        }
        self.ticks_to_skip -= 1;
        false
    }

    fn succeeded(&mut self) {
        self.resolved_at = Some(Instant::now());
        self.consecutive_failures = 0;
        self.ticks_to_skip = 0;
    }

    fn failed(&mut self) {
        self.consecutive_failures += 1;
        self.ticks_to_skip =
            (1u32 << (self.consecutive_failures - 1).min(31)).min(MAX_DISCOVERY_BACKOFF_TICKS);
    }

    fn invalidate(&mut self) {
        self.resolved_at = None;
    }
}

struct MonitorState {
    previous: BookableSlotSnapshot,
    suppress_next_publish: bool,
}

impl MonitorState {
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
