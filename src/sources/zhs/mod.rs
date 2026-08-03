//! ZHS Munich, the university sports booking system.
//!
//! One deployment, one set of credentials, courts declared in config. The
//! availability API is per-product, so the venue fetch fans out over the
//! venue's courts internally.

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

/// Builds the adapters for the configured ZHS venue.
///
/// Its courts come from config, so the catalog source is the shared
/// config-backed one rather than anything ZHS-specific.
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

/// The session every ZHS request is made under.
///
/// The credentials are demanded here rather than at startup, so a deployment
/// with no ZHS venue needs none — which is why they arrive as an `Option`.
fn authenticate(config: &Config, credentials: Option<Credentials>) -> Result<Auth> {
    let credentials = credentials.context(
        "a ZHS venue is configured, so COURT_ALERT_EMAIL and COURT_ALERT_PASSWORD must be set",
    )?;
    let venue = super::only_venue(config, Provider::Zhs)?;
    let VenueIdentity::Zhs { base_url } = &venue.identity else {
        // `only_venue` filtered on the provider, which the identity defines.
        unreachable!("a ZHS venue always carries a ZHS identity");
    };
    Auth::new(base_url.clone(), credentials)
}
