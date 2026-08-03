mod api;
mod auth;
mod dto;
#[cfg(test)]
mod testing;

pub use api::ZhsSlotAvailabilitySource;
pub use auth::Auth;

use std::sync::Arc;

use anyhow::{Context, Result};

use crate::config::{Config, Credentials};
use crate::model::{Provider, VenueIdentity};
use crate::ports::{CourtCatalogSource, VenueAvailabilitySource};

pub(super) fn build(
    config: &Config,
    credentials: Option<Credentials>,
    configured_catalogs: Arc<dyn CourtCatalogSource>,
) -> Result<(
    Arc<dyn VenueAvailabilitySource>,
    Arc<dyn CourtCatalogSource>,
)> {
    let auth = authenticate(config, credentials)?;
    Ok((
        Arc::new(ZhsSlotAvailabilitySource::new(auth)),
        configured_catalogs,
    ))
}

fn authenticate(config: &Config, credentials: Option<Credentials>) -> Result<Auth> {
    let credentials = credentials.context(
        "a ZHS venue is configured, so COURT_ALERT_EMAIL and COURT_ALERT_PASSWORD must be set",
    )?;
    let venue = super::only_venue(config, Provider::Zhs)?;
    let VenueIdentity::Zhs { base_url } = &venue.identity else {
        unreachable!("a ZHS venue always carries a ZHS identity");
    };
    Auth::new(base_url.clone(), credentials)
}
