use std::collections::HashMap;
use std::sync::Arc;

use uuid::Uuid;

use super::{CourtAttributes, CourtCatalog, Sport, VenueId};

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

/// Every venue's court catalog, keyed by venue.
///
/// Read by the monitor loops, subscription matching, command validation and
/// rendering; written only by a venue's own loop after discovery.
#[derive(Debug, Default)]
pub struct VenueRegistry {
    sports: HashMap<VenueId, Sport>,
    catalogs: HashMap<VenueId, CatalogState>,
}

impl VenueRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a venue with no catalog yet.
    pub fn register(&mut self, venue_id: VenueId, sport: Sport) {
        self.sports.insert(venue_id.clone(), sport);
        self.catalogs
            .entry(venue_id)
            .or_insert(CatalogState::Unresolved);
    }

    pub fn set_catalog(&mut self, venue_id: VenueId, catalog: CourtCatalog) {
        self.catalogs
            .insert(venue_id, CatalogState::Ready(Arc::new(catalog)));
    }

    pub fn state(&self, venue_id: &VenueId) -> Option<&CatalogState> {
        self.catalogs.get(venue_id)
    }

    /// The venue's catalog, or `None` while it is still unresolved.
    ///
    /// Returns an owned `Arc` so callers can drop the registry guard before
    /// awaiting: holding a read lock across a slow HTTP call would stall every
    /// other venue's loop.
    pub fn catalog(&self, venue_id: &VenueId) -> Option<Arc<CourtCatalog>> {
        match self.catalogs.get(venue_id) {
            Some(CatalogState::Ready(catalog)) => Some(catalog.clone()),
            _ => None,
        }
    }

    pub fn sport(&self, venue_id: &VenueId) -> Option<Sport> {
        self.sports.get(venue_id).copied()
    }

    pub fn attributes_of(&self, venue_id: &VenueId, court_id: Uuid) -> Option<CourtAttributes> {
        match self.catalogs.get(venue_id) {
            Some(CatalogState::Ready(catalog)) => catalog.attributes_of(court_id).cloned(),
            _ => None,
        }
    }

    /// Every resolved venue of a sport, in stable venue-id order.
    pub fn catalogs_for_sport(&self, sport: Sport) -> Vec<(VenueId, Arc<CourtCatalog>)> {
        let mut found: Vec<(VenueId, Arc<CourtCatalog>)> = self
            .sports
            .iter()
            .filter(|(_, venue_sport)| **venue_sport == sport)
            .filter_map(|(venue_id, _)| {
                self.catalog(venue_id)
                    .map(|catalog| (venue_id.clone(), catalog))
            })
            .collect();
        found.sort_by(|(left, _), (right, _)| left.cmp(right));
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Court, CourtSurface};

    fn venue() -> VenueId {
        VenueId::new("zhs-munich")
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
        registry.register(venue(), Sport::Tennis);

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
        registry.register(venue(), Sport::Tennis);
        registry.set_catalog(venue(), catalog());

        assert_eq!(registry.catalog(&venue()).unwrap().courts().len(), 1);
        assert_eq!(
            registry.attributes_of(&venue(), Uuid::from_u128(2)),
            Some(CourtAttributes::tennis(CourtSurface::Clay))
        );
    }

    #[test]
    fn re_registering_keeps_an_already_resolved_catalog() {
        let mut registry = VenueRegistry::new();
        registry.register(venue(), Sport::Tennis);
        registry.set_catalog(venue(), catalog());
        registry.register(venue(), Sport::Tennis);

        assert!(registry.catalog(&venue()).is_some());
    }

    #[test]
    fn unknown_venues_and_courts_resolve_to_nothing() {
        let mut registry = VenueRegistry::new();
        registry.register(venue(), Sport::Tennis);
        registry.set_catalog(venue(), catalog());

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
        registry.register(venue(), Sport::Tennis);
        registry.set_catalog(venue(), catalog());
        registry.register(VenueId::new("a-padel-club"), Sport::Padel);

        let tennis = registry.catalogs_for_sport(Sport::Tennis);
        assert_eq!(tennis.len(), 1);
        assert_eq!(tennis[0].0, venue());
        assert!(registry.catalogs_for_sport(Sport::Padel).is_empty());
    }
}
