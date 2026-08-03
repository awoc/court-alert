mod configured;
pub mod playtomic;
pub mod zhs;

pub use configured::ConfiguredCatalogSource;

use std::sync::Arc;

use anyhow::{Context, Result};

use crate::config::{Config, Credentials};
use crate::model::Provider;
use crate::ports::ProviderSources;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// The furthest ahead the provider will answer for, when it publishes a
    /// horizon. `None` means it does not, so only the global limit applies.
    pub max_lookahead_days: Option<i64>,
}

impl Provider {
    pub fn capabilities(self) -> Capabilities {
        match self {
            // ZHS publishes no horizon; the booking window closes per slot.
            Provider::Zhs => Capabilities {
                max_lookahead_days: None,
            },
            Provider::Playtomic => Capabilities {
                max_lookahead_days: Some(playtomic::MAX_LOOKAHEAD_DAYS),
            },
        }
    }
}

pub fn build(config: &Config, credentials: Option<Credentials>) -> Result<ProviderSources> {
    let mut sources = ProviderSources::new();
    let configured = Arc::new(ConfiguredCatalogSource::new(config.catalogs().clone()));

    if config.uses(Provider::Zhs) {
        let (availability, catalogs) = zhs::build(config, credentials, configured.clone())?;
        sources.insert(Provider::Zhs, availability, catalogs);
    }
    if config.uses(Provider::Playtomic) {
        let (availability, catalogs) = playtomic::build()?;
        sources.insert(Provider::Playtomic, availability, catalogs);
    }

    Ok(sources)
}

pub fn validate_configuration(config: &Config) -> Result<()> {
    for venue in config.venues() {
        // Beyond a provider's own horizon every extra day is a request that can
        // only come back empty.
        let Some(cap) = venue.provider().capabilities().max_lookahead_days else {
            continue;
        };
        let requested = config.lookahead_days_for(venue);
        anyhow::ensure!(
            requested <= cap,
            "venue {}: {} serves at most {cap} days, but lookahead_days is {requested}",
            venue.id,
            venue.provider(),
        );
    }
    Ok(())
}

/// ZHS is per-deployment rather than per-club: one set of credentials, one
/// base URL, so a second venue would need its own of both.
fn only_venue(config: &Config, provider: Provider) -> Result<&crate::model::Venue> {
    let mut venues = config
        .venues()
        .iter()
        .filter(|venue| venue.provider() == provider);
    let first = venues
        .next()
        .with_context(|| format!("no {provider} venue configured"))?;
    let extra = venues.count();
    anyhow::ensure!(
        extra == 0,
        "only one {provider} venue is supported (found {}); \
         a second would need its own credentials",
        extra + 1
    );
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZHS_ONLY: &str = r#"
poll_interval_secs = 300
lookahead_days = 7

[[venues]]
id = "zhs-munich"
display_name = "ZHS München"
sport = "tennis"
provider = "zhs"
base_url = "https://kurse.zhs-muenchen.de"

  [[venues.courts]]
  id = "92db7384-2dec-4888-a92a-4c2b6faac5f7"
  name = "Tennis Court 1"
"#;

    const PADEL_ONLY: &str = r#"
poll_interval_secs = 300
lookahead_days = 15

[[venues]]
id = "casa-padel"
display_name = "Casa Padel"
sport = "padel"
provider = "playtomic"
tenant_id = "f8483f72-1d14-49eb-a98b-e4b89d969c78"
slug = "casa-padel"
"#;

    fn credentials() -> Credentials {
        Credentials {
            email: "alice@example.com".into(),
            password: "hunter2".into(),
        }
    }

    #[test]
    fn a_playtomic_only_deployment_needs_no_credentials() {
        let config = Config::parse(PADEL_ONLY).expect("parse");
        assert!(build(&config, None).is_ok());
    }

    #[test]
    fn a_zhs_venue_without_credentials_names_the_missing_variables() {
        let config = Config::parse(ZHS_ONLY).expect("parse");

        // Not `expect_err`: `ProviderSources` holds trait objects and has no
        // `Debug`, so the success arm cannot be formatted.
        let error = match build(&config, None) {
            Ok(_) => panic!("missing credentials must be rejected"),
            Err(error) => error,
        };

        let message = format!("{error:#}");
        assert!(message.contains("COURT_ALERT_EMAIL"), "got: {message}");
        assert!(message.contains("COURT_ALERT_PASSWORD"), "got: {message}");
    }

    #[test]
    fn a_zhs_venue_with_credentials_wires_up() {
        let config = Config::parse(ZHS_ONLY).expect("parse");
        assert!(build(&config, Some(credentials())).is_ok());
    }

    /// By the number, not by prose: Playtomic serves today…today+14
    /// *inclusive*, and the window is half-open, so 15 covers exactly the
    /// horizon and 16 adds a guaranteed-empty request.
    #[test]
    fn a_playtomic_venue_may_look_ahead_fifteen_days_but_not_sixteen() {
        let with = |days: i64| {
            let config = Config::parse(
                &PADEL_ONLY.replace("lookahead_days = 15", &format!("lookahead_days = {days}")),
            )
            .expect("parse");
            validate_configuration(&config)
        };

        assert!(with(15).is_ok(), "15 days is exactly the horizon");
        let error = with(16).expect_err("16 days must be rejected");
        assert!(
            format!("{error:#}").contains("15"),
            "the cap is not named: {error:#}"
        );
    }

    #[test]
    fn a_zhs_venue_is_not_capped_at_the_playtomic_horizon() {
        let config = Config::parse(&ZHS_ONLY.replace("lookahead_days = 7", "lookahead_days = 30"))
            .expect("parse");

        assert!(validate_configuration(&config).is_ok());
    }

    #[test]
    fn a_second_zhs_venue_is_rejected_because_it_would_need_its_own_credentials() {
        let twin = format!(
            "{ZHS_ONLY}\n[[venues]]\nid = \"zhs-other\"\ndisplay_name = \"Elsewhere\"\n\
             sport = \"tennis\"\nprovider = \"zhs\"\nbase_url = \"https://example.test\"\n\n  \
             [[venues.courts]]\n  id = \"11111111-2222-3333-4444-555555555555\"\n  \
             name = \"Court 9\"\n"
        );
        let config = Config::parse(&twin).expect("parse");

        let error = match build(&config, Some(credentials())) {
            Ok(_) => panic!("a second ZHS venue must be rejected"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("only one"), "got: {error:#}");
    }
}
