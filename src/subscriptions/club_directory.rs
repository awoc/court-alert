use std::sync::{Arc, RwLock};

use crate::model::{Sport, VenueRegistry};

use super::contract::{ClubRef, ClubSummary, CourtSummary};

#[derive(Clone)]
pub struct ClubDirectory {
    registry: Arc<RwLock<VenueRegistry>>,
}

impl ClubDirectory {
    pub fn new(registry: Arc<RwLock<VenueRegistry>>) -> Self {
        Self { registry }
    }

    pub fn clubs_of(&self, sport: Sport) -> Vec<ClubRef> {
        self.summaries_of(sport)
            .into_iter()
            .map(|club| ClubRef {
                venue_id: club.venue_id,
                name: club.name,
            })
            .collect()
    }

    pub fn summaries_of(&self, sport: Sport) -> Vec<ClubSummary> {
        let registry = self.registry.read().expect("venue registry poisoned");
        registry
            .venues_of_sport(sport)
            .into_iter()
            .map(|venue_id| {
                let courts = registry
                    .catalog(&venue_id)
                    .map(|catalog| catalog.courts().iter().map(CourtSummary::from).collect())
                    .unwrap_or_default();
                ClubSummary {
                    name: registry.club_label(&venue_id),
                    venue_id,
                    sport,
                    courts,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::model::{CourtCatalog, VenueId};
    use crate::subscriptions::testing::{
        PADEL_CLUB_NAME, clay_court, padel_club, padel_court, registry_of, tennis_club,
    };

    #[test]
    fn clubs_of_names_only_the_clubs_of_that_sport() {
        let directory = ClubDirectory::new(registry_of(vec![
            (padel_club("padel"), Some(vec![padel_court(1, "Centre")])),
            (tennis_club("tennis"), Some(vec![clay_court(2, "Court 2")])),
        ]));

        assert_eq!(
            directory.clubs_of(Sport::Padel),
            vec![ClubRef {
                venue_id: VenueId::new("padel"),
                name: PADEL_CLUB_NAME.into(),
            }]
        );
    }

    #[test]
    fn a_club_whose_catalog_is_not_ready_is_listed_without_courts() {
        let directory = ClubDirectory::new(registry_of(vec![(padel_club("padel"), None)]));

        let clubs = directory.summaries_of(Sport::Padel);

        assert_eq!(clubs.len(), 1);
        assert!(clubs[0].courts.is_empty());
    }

    #[test]
    fn a_catalog_that_arrives_later_shows_up_in_the_directory() {
        let registry = registry_of(vec![(padel_club("padel"), None)]);
        let directory = ClubDirectory::new(registry.clone());

        registry.write().unwrap().set_catalog(
            &VenueId::new("padel"),
            CourtCatalog::new(vec![padel_court(1, "Centre")]),
        );

        let clubs = directory.summaries_of(Sport::Padel);
        assert_eq!(clubs[0].courts[0].id, Uuid::from_u128(1));
    }
}
