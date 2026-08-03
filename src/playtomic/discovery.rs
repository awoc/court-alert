use anyhow::{Context, Result};
use async_trait::async_trait;
use tracing::{info, warn};

use crate::model::{Court, CourtAttributes, CourtCatalog, CourtLocation, Sport, Venue};
use crate::ports::CourtCatalogSource;

use super::rsc::{ClubPage, ResourceDto};
use super::{ClubDirectory, ClubMeta, PlaytomicClient, playtomic_sport_id};

pub struct PlaytomicCatalogSource {
    client: PlaytomicClient,
    directory: ClubDirectory,
}

impl PlaytomicCatalogSource {
    pub fn new(client: PlaytomicClient, directory: ClubDirectory) -> Self {
        Self { client, directory }
    }
}

#[async_trait]
impl CourtCatalogSource for PlaytomicCatalogSource {
    async fn discover(&self, venue: &Venue) -> Result<CourtCatalog> {
        let (tenant_id, slug) = super::playtomic_identity(venue)?;
        let body = self.client.club_page(slug).await?;
        let page = ClubPage::parse(&body)
            .with_context(|| format!("reading the club page of venue {}", venue.id))?;

        // A slug can be re-pointed at a different club, which would leave us
        // monitoring the wrong venue under the right name.
        anyhow::ensure!(
            page.tenant_id == tenant_id,
            "venue {}: club page for slug {slug:?} reports tenant {} but config says {tenant_id}",
            venue.id,
            page.tenant_id
        );

        // Recorded before the sport filter so the canary still has the club's
        // hours even if this venue's sport turns out to be absent.
        self.directory.record(
            venue.id.clone(),
            ClubMeta {
                timezone: page.timezone,
                opening_hours: page
                    .opening_hours
                    .into_iter()
                    .map(|(day, hours)| (day, hours.opening_time))
                    .collect(),
            },
        );

        let wanted = playtomic_sport_id(venue.sport);
        let (mine, theirs): (Vec<_>, Vec<_>) = page
            .resources
            .into_iter()
            .partition(|resource| resource.sport.eq_ignore_ascii_case(wanted));
        if !theirs.is_empty() {
            // Multi-sport clubs are real, and an unfiltered catalog would put
            // beach-volleyball courts in a padel venue.
            info!(
                venue = %venue.id,
                kept = mine.len(),
                skipped = theirs.len(),
                "club serves several sports; kept only its {wanted} courts"
            );
        }
        anyhow::ensure!(
            !mine.is_empty(),
            "venue {}: club page lists no {wanted} courts",
            venue.id
        );

        Ok(CourtCatalog::new(
            mine.iter()
                .map(|resource| court_from(venue.sport, resource))
                .collect(),
        ))
    }
}

fn court_from(sport: Sport, resource: &ResourceDto) -> Court {
    let attributes = match sport {
        Sport::Padel => CourtAttributes::padel(location_from(&resource.features)),
        // ZHS supplies no location data, so tennis has nowhere to put one yet.
        Sport::Tennis => CourtAttributes::tennis(Default::default()),
    };
    Court::new(resource.resource_id, resource.name.clone(), attributes)
}

/// Maps the club's `features` vocabulary onto the one attribute anyone filters
/// on. The rest (`double`/`single`, `crystal`/`panoramic`, `wall`) is read and
/// discarded.
fn location_from(features: &[String]) -> Option<CourtLocation> {
    let has = |wanted: &str| {
        features
            .iter()
            .any(|feature| feature.eq_ignore_ascii_case(wanted))
    };
    if has("outdoor") {
        Some(CourtLocation::Outdoor)
    } else if has("indoor") {
        Some(CourtLocation::Indoor)
    } else if has("roofed") {
        // A covered court is what someone filtering for "indoor" wants.
        Some(CourtLocation::Indoor)
    } else {
        None
    }
}

/// Compares the club's advertised opening time against the earliest slot the
/// availability API returned for a day.
///
/// A fixture-based test cannot catch a change in what `start_time` *means* — it
/// only re-encodes whatever the fixture was built with. This runs against live
/// data instead. Reading the times as club-local rather than UTC would place
/// the earliest start before the club opens, which is what this catches.
pub(super) fn check_opening_hours(
    venue_id: &str,
    page_timezone: &str,
    opening_time: Option<&str>,
    earliest_local: Option<chrono::NaiveTime>,
) {
    let (Some(opening), Some(earliest)) = (opening_time, earliest_local) else {
        return;
    };
    let Ok(opening) = chrono::NaiveTime::parse_from_str(opening, "%H:%M") else {
        warn!(venue = %venue_id, %opening, "club advertises an unparsable opening time");
        return;
    };
    if earliest < opening {
        warn!(
            venue = %venue_id,
            timezone = %page_timezone,
            error = %format!(
                "earliest available slot is {earliest} but the club opens at {opening}; \
                 the availability API's start_time may no longer be UTC"
            ),
            "opening-hours canary failed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(name: &str, sport: &str, features: &[&str]) -> ResourceDto {
        serde_json::from_value(serde_json::json!({
            "resourceId": uuid::Uuid::new_v4(),
            "name": name,
            "sport": sport,
            "features": features,
        }))
        .unwrap()
    }

    #[test]
    fn outdoor_and_indoor_map_straight_across() {
        assert_eq!(
            location_from(&["indoor".into(), "crystal".into()]),
            Some(CourtLocation::Indoor)
        );
        assert_eq!(
            location_from(&["outdoor".into(), "double".into()]),
            Some(CourtLocation::Outdoor)
        );
    }

    /// Six of 56 sampled courts carry `roofed` instead, e.g. iopadel's
    /// "Pista 1 (Cubierta)".
    #[test]
    fn roofed_counts_as_indoor() {
        assert_eq!(
            location_from(&["roofed".into(), "double".into()]),
            Some(CourtLocation::Indoor)
        );
    }

    /// Unknown rather than defaulted: a specific filter should exclude it, not
    /// silently mislabel it.
    #[test]
    fn a_court_with_no_location_feature_stays_unknown() {
        assert!(location_from(&["double".into(), "crystal".into()]).is_none());
        assert!(location_from(&[]).is_none());
    }

    /// `outdoor` wins so a court tagged both is never reported as indoor, which
    /// is the direction that would disappoint someone expecting a roof.
    #[test]
    fn an_explicit_outdoor_tag_wins_over_roofed() {
        assert_eq!(
            location_from(&["roofed".into(), "outdoor".into()]),
            Some(CourtLocation::Outdoor)
        );
    }

    #[test]
    fn a_padel_court_carries_its_location() {
        let court = court_from(Sport::Padel, &resource("Court 1", "PADEL", &["indoor"]));
        assert_eq!(
            court.attributes(),
            &CourtAttributes::padel(Some(CourtLocation::Indoor))
        );
    }

    #[test]
    fn the_canary_is_silent_when_the_first_slot_is_at_or_after_opening() {
        // Nothing to assert beyond "does not panic"; the warning is the output.
        check_opening_hours(
            "casa-padel",
            "Europe/Berlin",
            Some("07:00"),
            Some(chrono::NaiveTime::from_hms_opt(7, 0, 0).unwrap()),
        );
        check_opening_hours(
            "casa-padel",
            "Europe/Berlin",
            Some("07:00"),
            Some(chrono::NaiveTime::from_hms_opt(9, 30, 0).unwrap()),
        );
    }

    #[test]
    fn the_canary_tolerates_missing_inputs() {
        check_opening_hours("casa-padel", "Europe/Berlin", None, None);
        check_opening_hours("casa-padel", "Europe/Berlin", Some("nonsense"), None);
    }
}
