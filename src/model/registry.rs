use std::collections::HashMap;
use std::sync::Arc;

use uuid::Uuid;

use super::{CourtAttributes, CourtCatalog, Sport, Venue, VenueId};

/// Whether a venue's court catalog is known yet.
///
/// ZHS catalogs come from config and are `Ready` from the first moment.
/// Playtomic catalogs are scraped at runtime, so a venue starts `Unresolved`
/// and its loop is what fills it in — which is why the registry is shared
/// mutable state rather than a value handed out at startup.
#[derive(Debug, Clone)]
pub enum CatalogState {
    Unresolved,
    Ready(Arc<CourtCatalog>),
}

/// Written only by a venue's own loop after discovery; read by everything else.
///
/// One map, not one per fact: a catalog and a sport belong to the same venue,
/// and parallel maps would let them drift — a catalog under a venue with no
/// sport would be dropped silently by every sport-scoped consumer.
#[derive(Debug, Default)]
pub struct VenueRegistry {
    venues: HashMap<VenueId, VenueInfo>,
}

#[derive(Debug, Clone)]
struct VenueInfo {
    sport: Sport,
    display_name: String,
    catalog: CatalogState,
}

impl VenueRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a venue, keeping any catalog it has already resolved.
    pub fn register(&mut self, venue: &Venue) {
        let catalog = self
            .venues
            .get(&venue.id)
            .map_or(CatalogState::Unresolved, |known| known.catalog.clone());
        self.venues.insert(
            venue.id.clone(),
            VenueInfo {
                sport: venue.sport,
                display_name: venue.display_name.clone(),
                catalog,
            },
        );
    }

    /// Records a venue's courts and hands the stored catalog back, so a caller
    /// that needs it does not have to take a second lock to read its own write.
    ///
    /// `None` means the venue was never registered. That is a wiring bug rather
    /// than a runtime condition — inventing an entry for it would produce a
    /// venue with courts but no sport, which every sport-scoped consumer would
    /// then discard without saying why.
    pub fn set_catalog(
        &mut self,
        venue_id: &VenueId,
        catalog: CourtCatalog,
    ) -> Option<Arc<CourtCatalog>> {
        let known = self.venues.get_mut(venue_id)?;
        let catalog = Arc::new(catalog);
        known.catalog = CatalogState::Ready(catalog.clone());
        Some(catalog)
    }

    pub fn state(&self, venue_id: &VenueId) -> Option<&CatalogState> {
        self.venues.get(venue_id).map(|venue| &venue.catalog)
    }

    /// Returns an owned `Arc` so callers can drop the registry guard before
    /// awaiting: holding a read lock across a slow HTTP call would stall every
    /// other venue's loop.
    pub fn catalog(&self, venue_id: &VenueId) -> Option<Arc<CourtCatalog>> {
        match self.venues.get(venue_id).map(|venue| &venue.catalog) {
            Some(CatalogState::Ready(catalog)) => Some(catalog.clone()),
            _ => None,
        }
    }

    pub fn sport(&self, venue_id: &VenueId) -> Option<Sport> {
        self.venues.get(venue_id).map(|venue| venue.sport)
    }

    pub fn display_name(&self, venue_id: &VenueId) -> Option<&str> {
        self.venues
            .get(venue_id)
            .map(|venue| venue.display_name.as_str())
    }

    /// The club's name for a message, falling back to its id so an alert is
    /// never labelled with a blank.
    pub fn club_label(&self, venue_id: &VenueId) -> String {
        self.display_name(venue_id)
            .unwrap_or_else(|| venue_id.as_str())
            .to_owned()
    }

    /// Every venue of a sport, resolved or not, in stable venue-id order.
    pub fn venues_of_sport(&self, sport: Sport) -> Vec<VenueId> {
        let mut found: Vec<VenueId> = self
            .venues
            .iter()
            .filter(|(_, venue)| venue.sport == sport)
            .map(|(venue_id, _)| venue_id.clone())
            .collect();
        found.sort();
        found
    }

    pub fn attributes_of(&self, venue_id: &VenueId, court_id: Uuid) -> Option<CourtAttributes> {
        match self.venues.get(venue_id).map(|venue| &venue.catalog) {
            Some(CatalogState::Ready(catalog)) => catalog.attributes_of(court_id).cloned(),
            _ => None,
        }
    }

    /// Every resolved venue of a sport, in stable venue-id order.
    pub fn catalogs_for_sport(&self, sport: Sport) -> Vec<(VenueId, Arc<CourtCatalog>)> {
        self.venues_of_sport(sport)
            .into_iter()
            .filter_map(|venue_id| {
                self.catalog(&venue_id)
                    .map(|catalog| (venue_id.clone(), catalog))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Court, CourtSurface, VenueIdentity};

    fn tennis_venue() -> Venue {
        Venue {
            id: VenueId::new("zhs-munich"),
            display_name: "ZHS München".into(),
            sport: Sport::Tennis,
            identity: VenueIdentity::Zhs {
                base_url: "https://example.test".into(),
            },
            poll_interval_secs: None,
            lookahead_days: None,
            operating_window: None,
        }
    }

    fn padel_venue() -> Venue {
        Venue {
            id: VenueId::new("a-padel-club"),
            display_name: "A Padel Club".into(),
            sport: Sport::Padel,
            identity: VenueIdentity::Playtomic {
                tenant_id: Uuid::nil(),
                slug: "a-padel-club".into(),
            },
            poll_interval_secs: None,
            lookahead_days: None,
            operating_window: None,
        }
    }

    fn venue() -> VenueId {
        tennis_venue().id
    }

    fn catalog() -> CourtCatalog {
        CourtCatalog::new(vec![Court::new(
            Uuid::from_u128(2),
            "Court 2".into(),
            CourtAttributes::tennis(CourtSurface::Clay),
        )])
    }

    #[test]
    fn a_registered_venue_starts_unresolved() {
        let mut registry = VenueRegistry::new();
        registry.register(&tennis_venue());

        assert!(matches!(
            registry.state(&venue()),
            Some(CatalogState::Unresolved)
        ));
        assert!(registry.catalog(&venue()).is_none());
        assert_eq!(registry.sport(&venue()), Some(Sport::Tennis));
    }

    #[test]
    fn setting_a_catalog_resolves_the_venue() {
        let mut registry = VenueRegistry::new();
        registry.register(&tennis_venue());
        registry.set_catalog(&venue(), catalog());

        assert_eq!(registry.catalog(&venue()).unwrap().courts().len(), 1);
        assert_eq!(
            registry.attributes_of(&venue(), Uuid::from_u128(2)),
            Some(CourtAttributes::tennis(CourtSurface::Clay))
        );
    }

    #[test]
    fn re_registering_keeps_an_already_resolved_catalog() {
        let mut registry = VenueRegistry::new();
        registry.register(&tennis_venue());
        registry.set_catalog(&venue(), catalog());
        registry.register(&tennis_venue());

        assert!(registry.catalog(&venue()).is_some());
    }

    /// The invariant the single map exists to hold: courts and a sport belong
    /// to the same venue. Accepting a catalog for an unregistered venue would
    /// create one with courts but no sport, which every sport-scoped consumer
    /// — the broadcast guard, the matcher — would then discard without a word.
    #[test]
    fn a_catalog_cannot_be_stored_for_an_unregistered_venue() {
        let mut registry = VenueRegistry::new();

        assert!(
            registry
                .set_catalog(&VenueId::new("never-registered"), catalog())
                .is_none()
        );

        assert!(
            registry
                .catalog(&VenueId::new("never-registered"))
                .is_none()
        );
        assert!(registry.sport(&VenueId::new("never-registered")).is_none());
    }

    #[test]
    fn setting_a_catalog_returns_it() {
        let mut registry = VenueRegistry::new();
        registry.register(&tennis_venue());

        let stored = registry
            .set_catalog(&venue(), catalog())
            .expect("registered");

        assert_eq!(stored.courts().len(), 1);
        assert_eq!(stored, registry.catalog(&venue()).unwrap());
    }

    #[test]
    fn unknown_venues_and_courts_resolve_to_nothing() {
        let mut registry = VenueRegistry::new();
        registry.register(&tennis_venue());
        registry.set_catalog(&venue(), catalog());

        assert!(registry.attributes_of(&venue(), Uuid::nil()).is_none());
        assert!(
            registry
                .attributes_of(&VenueId::new("elsewhere"), Uuid::from_u128(2))
                .is_none()
        );
    }

    #[test]
    fn catalogs_for_sport_skips_other_sports_and_unresolved_venues() {
        let mut registry = VenueRegistry::new();
        registry.register(&tennis_venue());
        registry.set_catalog(&venue(), catalog());
        registry.register(&padel_venue());

        let tennis = registry.catalogs_for_sport(Sport::Tennis);
        assert_eq!(tennis.len(), 1);
        assert_eq!(tennis[0].0, venue());
        assert!(registry.catalogs_for_sport(Sport::Padel).is_empty());
    }

    #[test]
    fn venues_of_sport_lists_unresolved_venues_too() {
        let mut registry = VenueRegistry::new();
        registry.register(&tennis_venue());
        registry.register(&padel_venue());

        assert_eq!(
            registry.venues_of_sport(Sport::Padel),
            vec![padel_venue().id]
        );
        assert_eq!(registry.venues_of_sport(Sport::Tennis), vec![venue()]);
    }

    #[test]
    fn a_venue_reports_the_name_to_show_in_messages() {
        let mut registry = VenueRegistry::new();
        registry.register(&padel_venue());

        assert_eq!(
            registry.display_name(&padel_venue().id),
            Some("A Padel Club")
        );
        assert!(registry.display_name(&VenueId::new("elsewhere")).is_none());
    }
}
