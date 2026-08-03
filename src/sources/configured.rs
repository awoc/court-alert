use std::collections::HashMap;

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::model::{CourtCatalog, Venue, VenueId};
use crate::ports::CourtCatalogSource;

/// The catalog source for providers that declare their courts in config.
///
/// Discovery is a lookup, which keeps every venue's loop the same shape:
/// discover, then fetch.
pub struct ConfiguredCatalogSource {
    catalogs: HashMap<VenueId, CourtCatalog>,
}

impl ConfiguredCatalogSource {
    pub fn new(catalogs: HashMap<VenueId, CourtCatalog>) -> Self {
        Self { catalogs }
    }
}

#[async_trait]
impl CourtCatalogSource for ConfiguredCatalogSource {
    async fn discover(&self, venue: &Venue) -> Result<CourtCatalog> {
        self.catalogs
            .get(&venue.id)
            .cloned()
            .with_context(|| format!("venue {} declares no courts in config", venue.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Court, CourtAttributes, CourtSurface, Sport, VenueIdentity};
    use uuid::Uuid;

    fn venue(id: &str) -> Venue {
        Venue {
            id: VenueId::new(id),
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

    fn source() -> ConfiguredCatalogSource {
        ConfiguredCatalogSource::new(HashMap::from([(
            VenueId::new("zhs-munich"),
            CourtCatalog::new(vec![Court::new(
                Uuid::from_u128(2),
                "Court 2".into(),
                CourtAttributes::tennis(CourtSurface::Clay),
            )]),
        )]))
    }

    #[tokio::test]
    async fn discovery_hands_back_the_configured_catalog() {
        let catalog = source().discover(&venue("zhs-munich")).await.unwrap();
        assert_eq!(catalog.names(), vec!["Court 2".to_string()]);
    }

    #[tokio::test]
    async fn a_venue_with_no_configured_courts_is_an_error() {
        assert!(source().discover(&venue("elsewhere")).await.is_err());
    }
}
