use std::collections::BTreeMap;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::model::{CourtCatalog, SlotObservation, Venue};
use crate::ports::VenueAvailabilitySource;
use crate::time::berlin_dates_in;

use super::discovery::check_opening_hours;
use super::{ClubDirectory, PlaytomicClient, playtomic_sport_id};

#[derive(Debug, Deserialize)]
struct ResourceAvailabilityDto {
    resource_id: Uuid,
    start_date: NaiveDate,
    slots: Vec<SlotDto>,
}

#[derive(Debug, Deserialize)]
struct SlotDto {
    start_time: NaiveTime,
    /// Minutes. `price` is also present but has nowhere to live on the slot
    /// model, and neither the DM nor the message-edit path uses it.
    duration: i64,
}

pub struct PlaytomicAvailabilitySource {
    client: PlaytomicClient,
    directory: ClubDirectory,
}

impl PlaytomicAvailabilitySource {
    pub fn new(client: PlaytomicClient, directory: ClubDirectory) -> Self {
        Self { client, directory }
    }
}

#[async_trait]
impl VenueAvailabilitySource for PlaytomicAvailabilitySource {
    async fn fetch(
        &self,
        venue: &Venue,
        catalog: &CourtCatalog,
        starts_at: DateTime<Utc>,
        ends_at: DateTime<Utc>,
    ) -> Result<Vec<SlotObservation>> {
        let (tenant_id, _) = super::playtomic_identity(venue)?;
        let sport_id = playtomic_sport_id(venue.sport);
        let dates = berlin_dates_in(starts_at, ends_at);

        let mut observations = Vec::new();
        for (index, date) in dates.iter().enumerate() {
            // Sequentially, so the pooled TLS connection is reused: on the
            // armv7 deploy target the handshake dominates the CPU cost.
            if index > 0 {
                self.client.pause_between_dates().await;
            }
            let resources = self
                .client
                .availability(tenant_id, *date, sport_id)
                .await
                .with_context(|| {
                    format!("fetching availability for venue {} on {date}", venue.id)
                })?;
            let for_date = observations_for(venue, catalog, *date, resources);
            // Only the first date: a genuine break would otherwise warn once
            // per date, fifteen times a tick.
            if index == 0
                && let Some(meta) = self.directory.get(&venue.id)
            {
                run_canary(
                    venue.id.as_str(),
                    &meta.timezone,
                    meta.opening_time_on(*date),
                    *date,
                    &for_date,
                );
            }
            observations.extend(for_date);
        }

        debug!(
            venue = %venue.id,
            dates = dates.len(),
            slots = observations.len(),
            "playtomic venue polled"
        );
        Ok(observations)
    }
}

fn observations_for(
    venue: &Venue,
    catalog: &CourtCatalog,
    requested: NaiveDate,
    resources: Vec<ResourceAvailabilityDto>,
) -> Vec<SlotObservation> {
    // Keyed on the composed UTC instant, not on `(resource_id, start_time)`:
    // "18:00" recurs on every date, so the latter would fold a whole week of
    // fetches onto one slot. BTreeMap so the output order is deterministic.
    let mut shortest: BTreeMap<(Uuid, DateTime<Utc>), i64> = BTreeMap::new();

    for resource in resources {
        // The cheap guard that turns a date-semantics change into a visible
        // error rather than a silent off-by-one-day.
        if resource.start_date != requested {
            warn!(
                venue = %venue.id,
                requested = %requested,
                returned = %resource.start_date,
                error = "playtomic returned availability for a different date than requested",
                "playtomic date mismatch; skipping the response"
            );
            continue;
        }
        for slot in resource.slots {
            if slot.duration <= 0 {
                continue;
            }
            // `start_time` is already UTC — no timezone conversion. Reading it
            // as club-local would shift every alert by the club's offset.
            let starts_at = Utc.from_utc_datetime(&resource.start_date.and_time(slot.start_time));
            shortest
                .entry((resource.resource_id, starts_at))
                .and_modify(|shortest| *shortest = (*shortest).min(slot.duration))
                .or_insert(slot.duration);
        }
    }

    shortest
        .into_iter()
        .map(|((resource_id, starts_at), duration)| SlotObservation {
            venue_id: venue.id.clone(),
            court_id: resource_id,
            // An unknown resource keeps its bare UUID rather than vanishing, so
            // a newly added court still alerts — ugly but visible.
            court_name: catalog
                .courts()
                .iter()
                .find(|court| court.id() == resource_id)
                .map_or_else(|| resource_id.to_string(), |court| court.name().to_owned()),
            starts_at,
            // `ends_at` is the *shortest* bookable duration at this start, not
            // a fixed hour: a fifth of starts cannot be booked for 60 minutes,
            // and advertising 18:00–19:00 for one of those would be a lie.
            ends_at: starts_at + Duration::minutes(duration),
            // The API only lists what is free, so there is no booking deadline
            // to model — but a slot that has already started cannot be booked,
            // and this is the field `into_bookable` already checks for that.
            booking_closes_at: Some(starts_at),
            available_places: 1,
            already_booked: false,
            already_in_cart: false,
            already_on_waiting_list: false,
            blocked_by_resource: false,
        })
        .collect()
}

/// Runs the opening-hours canary over a day's observations.
pub(super) fn run_canary(
    venue_id: &str,
    timezone: &str,
    opening_time: Option<&str>,
    date: NaiveDate,
    observations: &[SlotObservation],
) {
    let Ok(tz) = timezone.parse::<Tz>() else {
        warn!(venue = %venue_id, %timezone, "club advertises an unknown timezone");
        return;
    };
    let earliest = observations
        .iter()
        .map(|observation| observation.starts_at.with_timezone(&tz))
        .min()
        // Comparing a time-of-day only makes sense if the slot lands on the
        // day it was requested for; a club open past local midnight would not.
        .filter(|local| local.date_naive() == date)
        .map(|local| local.time());
    check_opening_hours(venue_id, timezone, opening_time, earliest);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Court, CourtAttributes, CourtLocation, Sport, VenueId, VenueIdentity};

    const SAMPLE: &str = include_str!("testdata/availability.json");

    fn venue() -> Venue {
        Venue {
            id: VenueId::new("casa-padel"),
            display_name: "Casa Padel Pineapple Park".into(),
            sport: Sport::Padel,
            identity: VenueIdentity::Playtomic {
                tenant_id: Uuid::parse_str("f8483f72-1d14-49eb-a98b-e4b89d969c78").unwrap(),
                slug: "casa-padel-pineapple-park".into(),
            },
            poll_interval_secs: None,
            lookahead_days: None,
            operating_window: None,
        }
    }

    const COURT_1: &str = "b2d8552d-5794-4abc-879f-61d53f587978";
    const COURT_6: &str = "4f5332e6-d392-4d53-96af-6287e2d411b9";

    fn court_id(raw: &str) -> Uuid {
        Uuid::parse_str(raw).unwrap()
    }

    fn catalog() -> CourtCatalog {
        CourtCatalog::new(vec![
            Court::new(
                court_id(COURT_1),
                "Court 1 (Indoor)".into(),
                CourtAttributes::padel(Some(CourtLocation::Indoor)),
            ),
            Court::new(
                court_id(COURT_6),
                "Court 6 (Outdoor)".into(),
                CourtAttributes::padel(Some(CourtLocation::Outdoor)),
            ),
        ])
    }

    fn sample_date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 8, 5).unwrap()
    }

    fn parse(raw: &str) -> Vec<ResourceAvailabilityDto> {
        serde_json::from_str(raw).expect("fixture parses")
    }

    fn observations() -> Vec<SlotObservation> {
        observations_for(&venue(), &catalog(), sample_date(), parse(SAMPLE))
    }

    fn at(court: &str, hour: u32, minute: u32) -> Option<SlotObservation> {
        let wanted = Utc.with_ymd_and_hms(2026, 8, 5, hour, minute, 0).unwrap();
        observations()
            .into_iter()
            .find(|o| o.court_id == court_id(court) && o.starts_at == wanted)
    }

    /// The regression test for the two-hour bug: `start_time` is UTC, so
    /// 05:00:00 is 07:00 Berlin — exactly when the club opens.
    #[test]
    fn a_start_time_is_utc_and_renders_as_berlin_wall_clock() {
        let first = at(COURT_1, 5, 0).expect("05:00 UTC slot");

        assert_eq!(
            crate::time::fmt_berlin(first.starts_at),
            "Wed, 05.08.2026 07:00"
        );
    }

    /// Measured on a real day: a fifth of starts cannot be booked for an hour,
    /// so a hardcoded 60 would advertise a slot that does not exist.
    #[test]
    fn ends_at_uses_the_shortest_bookable_duration_at_that_start() {
        let hour = at(COURT_1, 5, 0).expect("05:00 slot offers 60");
        assert_eq!(hour.ends_at - hour.starts_at, Duration::minutes(60));

        let ninety = at(COURT_1, 8, 0).expect("08:00 slot's shortest is 90");
        assert_eq!(ninety.ends_at - ninety.starts_at, Duration::minutes(90));

        let only = at(COURT_6, 19, 0).expect("19:00 slot offers exactly one duration");
        assert_eq!(only.ends_at - only.starts_at, Duration::minutes(120));
    }

    #[test]
    fn durations_collapse_to_one_slot_per_start() {
        // The fixture holds more rows than starts precisely because most starts
        // offer several durations.
        let rows: usize = parse(SAMPLE).iter().map(|r| r.slots.len()).sum();
        let starts = observations().len();

        assert!(rows > starts, "fixture must exercise the collapse");
        assert_eq!(starts, 5);
    }

    /// The bug the composed-instant key exists to prevent: "18:00" recurs on
    /// every date, so keying on `(resource_id, start_time)` would fold a week
    /// of fetches onto a single slot.
    #[test]
    fn the_same_start_time_on_two_dates_stays_two_slots() {
        let monday = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
        let tuesday = NaiveDate::from_ymd_opt(2026, 8, 11).unwrap();
        let day = |date: NaiveDate| {
            let raw = format!(
                r#"[{{"resource_id":"{COURT_1}","start_date":"{date}",
                     "slots":[{{"start_time":"18:00:00","duration":60,"price":"44 EUR"}}]}}]"#
            );
            observations_for(&venue(), &catalog(), date, parse(&raw))
        };

        let mut both = day(monday);
        both.extend(day(tuesday));

        assert_eq!(both.len(), 2);
        assert_ne!(both[0].starts_at, both[1].starts_at);
    }

    #[test]
    fn known_resources_are_named_from_the_catalog() {
        assert_eq!(at(COURT_1, 5, 0).unwrap().court_name, "Court 1 (Indoor)");
    }

    /// A newly added court must still alert, ugly but visible, rather than
    /// silently vanishing from the poll.
    #[test]
    fn an_unknown_resource_keeps_its_bare_uuid() {
        let unknown = Uuid::from_u128(4242);
        let raw = format!(
            r#"[{{"resource_id":"{unknown}","start_date":"2026-08-05",
                 "slots":[{{"start_time":"09:00:00","duration":60}}]}}]"#
        );

        let observations = observations_for(&venue(), &catalog(), sample_date(), parse(&raw));

        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].court_name, unknown.to_string());
    }

    /// A silent off-by-one-day is worse than no data, so a mismatched
    /// `start_date` drops the response rather than shifting every slot.
    #[test]
    fn a_response_for_the_wrong_date_is_discarded() {
        let raw = format!(
            r#"[{{"resource_id":"{COURT_1}","start_date":"2026-08-06",
                 "slots":[{{"start_time":"09:00:00","duration":60}}]}}]"#
        );

        assert!(observations_for(&venue(), &catalog(), sample_date(), parse(&raw)).is_empty());
    }

    /// The API lists only what is free, so there is no booking deadline — but a
    /// slot that has already started cannot be booked either.
    #[test]
    fn a_slot_that_has_already_started_is_not_bookable() {
        let slot = at(COURT_1, 5, 0).unwrap();
        let before = Utc.with_ymd_and_hms(2026, 8, 5, 4, 0, 0).unwrap();
        let after = Utc.with_ymd_and_hms(2026, 8, 5, 6, 0, 0).unwrap();

        assert!(slot.clone().into_bookable(before).is_some());
        assert!(slot.into_bookable(after).is_none());
    }

    #[test]
    fn an_empty_response_yields_no_slots() {
        assert!(observations_for(&venue(), &catalog(), sample_date(), parse("[]")).is_empty());
    }
}
