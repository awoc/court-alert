use std::collections::HashMap;
use std::sync::Arc;

use crate::model::Provider;

use super::{CourtCatalogSource, VenueAvailabilitySource};

/// The adapters serving one provider.
#[derive(Clone)]
pub struct ProviderAdapters {
    pub availability: Arc<dyn VenueAvailabilitySource>,
    pub catalogs: Arc<dyn CourtCatalogSource>,
}

/// Which adapters serve which provider.
///
/// Lives with the ports rather than with either side: the monitor consumes it
/// and the booking adapters fill it, so neither has to depend on the other.
#[derive(Default)]
pub struct ProviderSources(HashMap<Provider, ProviderAdapters>);

impl ProviderSources {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &mut self,
        provider: Provider,
        availability: Arc<dyn VenueAvailabilitySource>,
        catalogs: Arc<dyn CourtCatalogSource>,
    ) {
        self.0.insert(
            provider,
            ProviderAdapters {
                availability,
                catalogs,
            },
        );
    }

    pub fn get(&self, provider: Provider) -> Option<&ProviderAdapters> {
        self.0.get(&provider)
    }
}
