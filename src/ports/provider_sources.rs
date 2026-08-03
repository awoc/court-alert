use std::collections::HashMap;
use std::sync::Arc;

use crate::model::Provider;

use super::{CourtCatalogSource, VenueAvailabilitySource};

#[derive(Clone)]
pub struct ProviderAdapters {
    pub availability: Arc<dyn VenueAvailabilitySource>,
    pub catalogs: Arc<dyn CourtCatalogSource>,
}

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
