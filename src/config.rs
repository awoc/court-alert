use std::collections::{HashMap, HashSet};
use std::env::VarError;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use uuid::Uuid;

use crate::model::{
    Court, CourtAttributes, CourtCatalog, CourtFilter, CourtLocation, CourtSurface,
    OperatingWindow, Sport, Venue, VenueId, VenueIdentity,
};

const MAX_LOOKAHEAD_DAYS: i64 = 366;
const DEFAULT_CONFIG_PATH: &str = "config.toml";
const DEFAULT_DB_PATH: &str = "data/court-alert.db";
const DEFAULT_WINDOW_START_HOUR: u32 = 8;
const DEFAULT_WINDOW_END_HOUR: u32 = 24;

#[derive(Debug, Clone)]
pub struct Config {
    poll_interval_secs: u64,
    lookahead_days: i64,
    quiet_first_poll: bool,
    operating_window: OperatingWindow,
    surface_filter: CourtFilter,
    venues: Vec<Venue>,
    /// Catalogs declared in config, i.e. ZHS venues only. Playtomic venues
    /// discover theirs at runtime and are absent here.
    catalogs: HashMap<VenueId, CourtCatalog>,
}

#[derive(Deserialize)]
struct ConfigFile {
    poll_interval_secs: u64,
    lookahead_days: i64,
    #[serde(default = "default_true")]
    quiet_first_poll: bool,
    #[serde(default = "default_window_start_hour")]
    operating_window_start_hour: u32,
    #[serde(default = "default_window_end_hour")]
    operating_window_end_hour: u32,
    #[serde(default = "default_surface_filter")]
    surface_filter: CourtFilter,
    #[serde(default)]
    venues: Vec<VenueEntry>,
    /// Retired in favour of `[[venues]]`; kept only to name the fix.
    #[serde(default)]
    products: Option<toml::Value>,
    /// Retired in favour of `[[venues]]`; kept only to name the fix.
    #[serde(default)]
    base_url: Option<String>,
}

#[derive(Deserialize)]
struct VenueEntry {
    id: VenueId,
    display_name: String,
    sport: Sport,
    #[serde(flatten)]
    identity: VenueIdentity,
    poll_interval_secs: Option<u64>,
    lookahead_days: Option<i64>,
    operating_window_start_hour: Option<u32>,
    operating_window_end_hour: Option<u32>,
    #[serde(default)]
    courts: Vec<CourtEntry>,
}

#[derive(Deserialize)]
struct CourtEntry {
    id: Uuid,
    name: String,
    surface: Option<CourtSurface>,
    location: Option<CourtLocation>,
}

fn default_true() -> bool {
    true
}

fn default_window_start_hour() -> u32 {
    DEFAULT_WINDOW_START_HOUR
}

fn default_window_end_hour() -> u32 {
    DEFAULT_WINDOW_END_HOUR
}

fn default_surface_filter() -> CourtFilter {
    CourtFilter::CLAY
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let file: ConfigFile = toml::from_str(&raw)
            .with_context(|| format!("parsing config file {}", path.display()))?;
        Self::from_file(file).with_context(|| format!("validating config file {}", path.display()))
    }

    #[cfg(test)]
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        Self::from_file(toml::from_str(raw)?)
    }

    fn from_file(file: ConfigFile) -> Result<Self> {
        anyhow::ensure!(
            file.products.is_none() && file.base_url.is_none(),
            "top-level `products` and `base_url` are no longer supported; \
             move them into a `[[venues]]` entry with \
             `provider = \"zhs\"` and its own `[[venues.courts]]` list \
             (keep `id = \"zhs-munich\"` to retain existing slots and subscriptions)"
        );
        anyhow::ensure!(
            !file.venues.is_empty(),
            "at least one `[[venues]]` is required"
        );

        let global_window = OperatingWindow::new(
            file.operating_window_start_hour,
            file.operating_window_end_hour,
        )?;

        let mut venues = Vec::with_capacity(file.venues.len());
        let mut catalogs = HashMap::new();
        let mut ids = HashSet::with_capacity(file.venues.len());
        for entry in file.venues {
            anyhow::ensure!(
                ids.insert(entry.id.clone()),
                "duplicate venue id {}",
                entry.id
            );
            let (venue, catalog) = validated_venue(entry)?;
            if let Some(catalog) = catalog {
                catalogs.insert(venue.id.clone(), catalog);
            }
            venues.push(venue);
        }

        Self {
            poll_interval_secs: file.poll_interval_secs,
            lookahead_days: file.lookahead_days,
            quiet_first_poll: file.quiet_first_poll,
            operating_window: global_window,
            surface_filter: file.surface_filter,
            venues,
            catalogs,
        }
        .validate()
    }

    fn validate(self) -> Result<Self> {
        anyhow::ensure!(
            self.poll_interval_secs > 0,
            "poll_interval_secs must be greater than zero"
        );
        anyhow::ensure!(
            (1..=MAX_LOOKAHEAD_DAYS).contains(&self.lookahead_days),
            "lookahead_days must be between 1 and {MAX_LOOKAHEAD_DAYS}"
        );
        for venue in &self.venues {
            if let Some(secs) = venue.poll_interval_secs {
                anyhow::ensure!(
                    secs > 0,
                    "venue {}: poll_interval_secs must be greater than zero",
                    venue.id
                );
            }
            if let Some(days) = venue.lookahead_days {
                anyhow::ensure!(
                    (1..=MAX_LOOKAHEAD_DAYS).contains(&days),
                    "venue {}: lookahead_days must be between 1 and {MAX_LOOKAHEAD_DAYS}",
                    venue.id
                );
            }
        }
        Ok(self)
    }

    pub fn poll_interval_secs(&self) -> u64 {
        self.poll_interval_secs
    }

    pub fn lookahead_days(&self) -> i64 {
        self.lookahead_days
    }

    pub fn quiet_first_poll(&self) -> bool {
        self.quiet_first_poll
    }

    pub fn operating_window(&self) -> OperatingWindow {
        self.operating_window
    }

    /// The broadcast (webhook) channel's filter. Tennis-only: padel alerts go
    /// out solely as `/padel` direct messages.
    pub fn surface_filter(&self) -> CourtFilter {
        self.surface_filter
    }

    pub fn venues(&self) -> &[Venue] {
        &self.venues
    }

    pub fn catalog_for(&self, venue_id: &VenueId) -> Option<&CourtCatalog> {
        self.catalogs.get(venue_id)
    }

    /// A venue's effective poll interval, its override or the global default.
    pub fn poll_interval_for(&self, venue: &Venue) -> u64 {
        venue.poll_interval_secs.unwrap_or(self.poll_interval_secs)
    }

    pub fn lookahead_days_for(&self, venue: &Venue) -> i64 {
        venue.lookahead_days.unwrap_or(self.lookahead_days)
    }

    pub fn operating_window_for(&self, venue: &Venue) -> OperatingWindow {
        venue.operating_window.unwrap_or(self.operating_window)
    }
}

fn validated_venue(entry: VenueEntry) -> Result<(Venue, Option<CourtCatalog>)> {
    let display_name = entry.display_name.trim().to_string();
    anyhow::ensure!(
        !display_name.is_empty(),
        "venue {} has a blank display_name",
        entry.id
    );

    let identity = match entry.identity {
        VenueIdentity::Zhs { base_url } => VenueIdentity::Zhs {
            base_url: validated_base_url(&base_url)
                .with_context(|| format!("venue {}", entry.id))?,
        },
        playtomic => playtomic,
    };

    let operating_window = match (
        entry.operating_window_start_hour,
        entry.operating_window_end_hour,
    ) {
        (None, None) => None,
        (start, end) => Some(
            OperatingWindow::new(
                start.unwrap_or(DEFAULT_WINDOW_START_HOUR),
                end.unwrap_or(DEFAULT_WINDOW_END_HOUR),
            )
            .with_context(|| format!("venue {}", entry.id))?,
        ),
    };

    let catalog = match identity {
        // Playtomic venues declare only `tenant_id` + `slug`; their courts are
        // discovered at runtime, so a court list here is a mistake worth naming.
        VenueIdentity::Playtomic { .. } => {
            anyhow::ensure!(
                entry.courts.is_empty(),
                "venue {}: Playtomic venues discover their courts at runtime, \
                 so `[[venues.courts]]` is not allowed",
                entry.id
            );
            None
        }
        VenueIdentity::Zhs { .. } => Some(
            validated_courts(&entry.id, entry.sport, entry.courts)
                .with_context(|| format!("venue {}", entry.id))?,
        ),
    };

    Ok((
        Venue {
            id: entry.id,
            display_name,
            sport: entry.sport,
            identity,
            poll_interval_secs: entry.poll_interval_secs,
            lookahead_days: entry.lookahead_days,
            operating_window,
        },
        catalog,
    ))
}

fn validated_base_url(raw: &str) -> Result<String> {
    let url = reqwest::Url::parse(raw.trim()).context("base_url is not a URL")?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https"),
        "base_url must use http or https"
    );
    anyhow::ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "base_url must not contain credentials, a query, or a fragment"
    );
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn validated_courts(
    venue_id: &VenueId,
    sport: Sport,
    entries: Vec<CourtEntry>,
) -> Result<CourtCatalog> {
    anyhow::ensure!(
        !entries.is_empty(),
        "venue {venue_id} must declare at least one `[[venues.courts]]`"
    );
    let mut ids = HashSet::with_capacity(entries.len());
    let mut numbers = HashSet::with_capacity(entries.len());
    let mut courts = Vec::with_capacity(entries.len());
    for entry in entries {
        let name = entry.name.trim().to_string();
        anyhow::ensure!(!name.is_empty(), "court {} has a blank name", entry.id);
        anyhow::ensure!(ids.insert(entry.id), "duplicate court id {}", entry.id);
        let court = Court::new(entry.id, name, court_attributes(sport, &entry)?);
        if let Some(number) = court.number() {
            anyhow::ensure!(
                numbers.insert(number),
                "duplicate court number {number} in court {:?}",
                court.name()
            );
        }
        courts.push(court);
    }
    Ok(CourtCatalog::new(courts))
}

/// A court's attributes must belong to its venue's sport: a `surface` key on a
/// padel venue is a config error, not something to ignore silently.
fn court_attributes(sport: Sport, entry: &CourtEntry) -> Result<CourtAttributes> {
    match sport {
        Sport::Tennis => {
            anyhow::ensure!(
                entry.location.is_none(),
                "court {:?} is at a tennis venue, so it takes `surface`, not `location`",
                entry.name
            );
            Ok(CourtAttributes::tennis(entry.surface.unwrap_or_default()))
        }
        Sport::Padel => {
            anyhow::ensure!(
                entry.surface.is_none(),
                "court {:?} is at a padel venue, so it takes `location`, not `surface`",
                entry.name
            );
            Ok(CourtAttributes::padel(entry.location))
        }
    }
}

pub struct Credentials {
    pub email: String,
    pub password: String,
}

pub struct Settings {
    pub config_path: PathBuf,
    pub db_path: PathBuf,
    pub credentials: Credentials,
    pub discord_webhook: Option<reqwest::Url>,
    pub discord_error_webhook: Option<reqwest::Url>,
    pub discord_bot: Option<DiscordSettings>,
}

#[derive(Debug, Clone)]
pub struct DiscordSettings {
    pub token: String,
    pub guild_id: Option<u64>,
    pub admin_ids: HashSet<String>,
}

impl Settings {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            config_path: env_path("COURT_ALERT_CONFIG_PATH", DEFAULT_CONFIG_PATH),
            db_path: env_path("COURT_ALERT_DB_PATH", DEFAULT_DB_PATH),
            credentials: Credentials {
                email: std::env::var("COURT_ALERT_EMAIL")
                    .context("COURT_ALERT_EMAIL environment variable not set")?,
                password: std::env::var("COURT_ALERT_PASSWORD")
                    .context("COURT_ALERT_PASSWORD environment variable not set")?,
            },
            discord_webhook: env_parsed("COURT_ALERT_DISCORD_WEBHOOK")?,
            discord_error_webhook: env_parsed("COURT_ALERT_DISCORD_ERROR_WEBHOOK")?,
            discord_bot: discord_from_env()?,
        })
    }
}

fn discord_from_env() -> Result<Option<DiscordSettings>> {
    let guild_id: Option<u64> = env_parsed("COURT_ALERT_DISCORD_GUILD_ID")?;
    let admin_ids = split_ids(&std::env::var("COURT_ALERT_DISCORD_ADMIN_IDS").unwrap_or_default());

    let Some(token) = std::env::var("COURT_ALERT_DISCORD_BOT_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };

    Ok(Some(DiscordSettings {
        token,
        guild_id,
        admin_ids,
    }))
}

fn split_ids(raw: &str) -> HashSet<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn env_path(name: &str, default: &str) -> PathBuf {
    std::env::var_os(name)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}

fn env_parsed<T>(name: &str) -> Result<Option<T>>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    match std::env::var(name) {
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => anyhow::bail!("{name} contains non-Unicode data"),
        Ok(s) if s.trim().is_empty() => Ok(None),
        Ok(s) => s
            .trim()
            .parse()
            .map(Some)
            .with_context(|| format!("parsing {name}={s:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Provider;

    const SAMPLE: &str = r#"
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

  [[venues.courts]]
  id = "11111111-2222-3333-4444-555555555555"
  name = "Tennis Court 2"
"#;

    const PADEL_VENUE: &str = r#"
[[venues]]
id = "casa-padel-pineapple-park"
display_name = "Casa Padel Pineapple Park"
sport = "padel"
provider = "playtomic"
tenant_id = "f8483f72-1d14-49eb-a98b-e4b89d969c78"
slug = "casa-padel-pineapple-park"
"#;

    fn venue<'a>(cfg: &'a Config, id: &str) -> &'a Venue {
        cfg.venues()
            .iter()
            .find(|venue| venue.id.as_str() == id)
            .unwrap_or_else(|| panic!("venue {id} missing"))
    }

    fn courts<'a>(cfg: &'a Config, id: &str) -> &'a [Court] {
        cfg.catalog_for(&VenueId::new(id))
            .unwrap_or_else(|| panic!("catalog for {id} missing"))
            .courts()
    }

    #[test]
    fn parses_config() {
        let cfg = Config::parse(SAMPLE).expect("parse");
        assert_eq!(cfg.poll_interval_secs(), 300);
        assert_eq!(cfg.lookahead_days(), 7);

        let zhs = venue(&cfg, "zhs-munich");
        assert_eq!(zhs.display_name, "ZHS München");
        assert_eq!(zhs.sport, Sport::Tennis);
        assert_eq!(
            zhs.identity,
            VenueIdentity::Zhs {
                base_url: "https://kurse.zhs-muenchen.de".into()
            }
        );

        let courts = courts(&cfg, "zhs-munich");
        assert_eq!(courts.len(), 2);
        assert_eq!(courts[0].name(), "Tennis Court 1");
        assert_eq!(
            courts[0].id(),
            Uuid::parse_str("92db7384-2dec-4888-a92a-4c2b6faac5f7").unwrap()
        );
    }

    #[test]
    fn parses_a_playtomic_venue_without_a_court_list() {
        let cfg = Config::parse(&format!("{SAMPLE}{PADEL_VENUE}")).expect("parse");

        let padel = venue(&cfg, "casa-padel-pineapple-park");
        assert_eq!(padel.sport, Sport::Padel);
        assert_eq!(padel.provider(), Provider::Playtomic);
        assert_eq!(
            padel.identity,
            VenueIdentity::Playtomic {
                tenant_id: Uuid::parse_str("f8483f72-1d14-49eb-a98b-e4b89d969c78").unwrap(),
                slug: "casa-padel-pineapple-park".into(),
            }
        );
        // Its catalog is discovered at runtime, so config supplies none.
        assert!(cfg.catalog_for(&padel.id).is_none());
    }

    #[test]
    fn a_playtomic_venue_may_not_declare_courts() {
        let with_courts = format!(
            "{SAMPLE}{PADEL_VENUE}\n  [[venues.courts]]\n  id = \
             \"22222222-2222-3333-4444-555555555555\"\n  name = \"Court 1\"\n"
        );
        assert!(Config::parse(&with_courts).is_err());
    }

    #[test]
    fn a_retired_flat_config_is_rejected_with_the_fix_named() {
        let legacy = r#"
poll_interval_secs = 300
lookahead_days = 7
base_url = "https://kurse.zhs-muenchen.de"

[[products]]
id = "92db7384-2dec-4888-a92a-4c2b6faac5f7"
name = "Tennis Court 1"
"#;
        let error = Config::parse(legacy).expect_err("legacy config must be rejected");
        let message = format!("{error:#}");
        assert!(
            message.contains("[[venues]]"),
            "unhelpful message: {message}"
        );
        assert!(
            message.contains("zhs-munich"),
            "the id to keep is not named: {message}"
        );
    }

    #[test]
    fn a_config_without_venues_is_rejected() {
        assert!(Config::parse("poll_interval_secs = 300\nlookahead_days = 7\n").is_err());
    }

    #[test]
    fn validation_rejects_duplicate_venue_ids() {
        let duplicate = format!(
            "{SAMPLE}\n[[venues]]\nid = \"zhs-munich\"\ndisplay_name = \"Twin\"\n\
             sport = \"tennis\"\nprovider = \"zhs\"\n\
             base_url = \"https://example.test\"\n\n  [[venues.courts]]\n  \
             id = \"33333333-2222-3333-4444-555555555555\"\n  name = \"Court 9\"\n"
        );
        assert!(Config::parse(&duplicate).is_err());
    }

    #[test]
    fn per_venue_overrides_fall_back_to_the_globals() {
        let cfg = Config::parse(&format!(
            "{SAMPLE}{PADEL_VENUE}poll_interval_secs = 120\nlookahead_days = 15\n\
             operating_window_start_hour = 7\n"
        ))
        .expect("parse");

        let zhs = venue(&cfg, "zhs-munich");
        assert_eq!(cfg.poll_interval_for(zhs), 300);
        assert_eq!(cfg.lookahead_days_for(zhs), 7);
        assert_eq!(cfg.operating_window_for(zhs), cfg.operating_window());

        let padel = venue(&cfg, "casa-padel-pineapple-park");
        assert_eq!(cfg.poll_interval_for(padel), 120);
        assert_eq!(cfg.lookahead_days_for(padel), 15);
        assert_eq!(
            cfg.operating_window_for(padel),
            OperatingWindow::new(7, 24).unwrap()
        );
    }

    #[test]
    fn a_tennis_venue_takes_a_surface_and_a_padel_venue_a_location() {
        let with_location = SAMPLE.replace(
            "name = \"Tennis Court 2\"",
            "name = \"Tennis Court 2\"\n  location = \"indoor\"",
        );
        assert!(
            Config::parse(&with_location).is_err(),
            "a tennis court must not take `location`"
        );

        // The mirror case cannot arise through config today, because Playtomic
        // venues (the only padel ones) declare no courts at all.
        let padel_with_surface = format!(
            "{SAMPLE}\n[[venues]]\nid = \"padel-hall\"\ndisplay_name = \"Padel Hall\"\n\
             sport = \"padel\"\nprovider = \"zhs\"\nbase_url = \"https://example.test\"\n\n  \
             [[venues.courts]]\n  id = \"44444444-2222-3333-4444-555555555555\"\n  \
             name = \"Court 1\"\n  surface = \"clay\"\n"
        );
        assert!(Config::parse(&padel_with_surface).is_err());
    }

    #[test]
    fn quiet_first_poll_defaults_to_true() {
        let cfg = Config::parse(SAMPLE).expect("parse");
        assert!(cfg.quiet_first_poll());
    }

    #[test]
    fn quiet_first_poll_can_be_disabled() {
        let raw = format!("quiet_first_poll = false\n{SAMPLE}");
        let cfg = Config::parse(&raw).expect("parse");
        assert!(!cfg.quiet_first_poll());
    }

    #[test]
    fn operating_window_defaults_to_8_to_24() {
        let cfg = Config::parse(SAMPLE).expect("parse");
        assert_eq!(cfg.operating_window(), OperatingWindow::new(8, 24).unwrap());
    }

    #[test]
    fn operating_window_must_be_within_a_single_day_and_nonempty() {
        let configured =
            format!("operating_window_start_hour = 6\noperating_window_end_hour = 22\n{SAMPLE}");
        let cfg = Config::parse(&configured).expect("parse");
        assert_eq!(cfg.operating_window(), OperatingWindow::new(6, 22).unwrap());

        assert!(Config::parse(&configured.replace("start_hour = 6", "start_hour = 22")).is_err());
        assert!(Config::parse(&configured.replace("end_hour = 22", "end_hour = 25")).is_err());
    }

    #[test]
    fn a_venue_may_override_the_operating_window() {
        let configured = SAMPLE.replace(
            "base_url = \"https://kurse.zhs-muenchen.de\"",
            "base_url = \"https://kurse.zhs-muenchen.de\"\n\
             operating_window_start_hour = 7",
        );
        let cfg = Config::parse(&configured).expect("parse");
        assert_eq!(
            cfg.operating_window_for(venue(&cfg, "zhs-munich")),
            OperatingWindow::new(7, 24).unwrap()
        );

        assert!(Config::parse(&configured.replace("start_hour = 7", "start_hour = 24")).is_err());
    }

    fn validated(raw: &str) -> Result<Config> {
        Config::parse(raw)
    }

    #[test]
    fn validation_rejects_unsafe_poll_settings() {
        assert!(
            validated(&SAMPLE.replace("poll_interval_secs = 300", "poll_interval_secs = 0"))
                .is_err()
        );
        assert!(validated(&SAMPLE.replace("lookahead_days = 7", "lookahead_days = -1")).is_err());
        assert!(validated(&SAMPLE.replace("lookahead_days = 7", "lookahead_days = 367")).is_err());
    }

    #[test]
    fn validation_rejects_invalid_court_collections() {
        let empty = SAMPLE.split("  [[venues.courts]]").next().unwrap();
        assert!(validated(empty).is_err());

        let duplicate = SAMPLE.replace(
            "11111111-2222-3333-4444-555555555555",
            "92db7384-2dec-4888-a92a-4c2b6faac5f7",
        );
        assert!(validated(&duplicate).is_err());

        assert!(validated(&SAMPLE.replace("Tennis Court 1", "   ")).is_err());
        assert!(validated(&SAMPLE.replace("ZHS München", "  ")).is_err());
    }

    #[test]
    fn validation_rejects_duplicate_court_numbers() {
        assert!(
            validated(&SAMPLE.replace("Tennis Court 2", "Tennis Court 1 - Synthetic")).is_err()
        );
    }

    /// Court numbers only have to be unique within a venue — two clubs may both
    /// have a "Court 1".
    #[test]
    fn the_same_court_number_may_appear_at_two_venues() {
        let second = format!(
            "{SAMPLE}\n[[venues]]\nid = \"other-club\"\ndisplay_name = \"Other Club\"\n\
             sport = \"tennis\"\nprovider = \"zhs\"\nbase_url = \"https://example.test\"\n\n  \
             [[venues.courts]]\n  id = \"55555555-2222-3333-4444-555555555555\"\n  \
             name = \"Tennis Court 1\"\n"
        );
        assert!(Config::parse(&second).is_ok());
    }

    #[test]
    fn courts_are_clay_unless_they_say_otherwise() {
        let cfg = Config::parse(SAMPLE).expect("parse");
        let clay = CourtAttributes::tennis(CourtSurface::Clay);
        assert_eq!(courts(&cfg, "zhs-munich")[0].attributes(), &clay);

        let configured = SAMPLE.replace(
            "name = \"Tennis Court 2\"",
            "name = \"Tennis Court 2\"\n  surface = \"synthetic\"",
        );
        let cfg = Config::parse(&configured).expect("parse");
        let courts = courts(&cfg, "zhs-munich");
        assert_eq!(courts[0].attributes(), &clay);
        assert_eq!(
            courts[1].attributes(),
            &CourtAttributes::tennis(CourtSurface::Synthetic)
        );
    }

    #[test]
    fn validation_rejects_an_unknown_court_surface() {
        let configured = SAMPLE.replace(
            "name = \"Tennis Court 2\"",
            "name = \"Tennis Court 2\"\n  surface = \"grass\"",
        );
        assert!(validated(&configured).is_err());
    }

    #[test]
    fn surface_filter_defaults_to_clay() {
        let cfg = Config::parse(SAMPLE).expect("parse");
        assert_eq!(cfg.surface_filter(), CourtFilter::CLAY);
    }

    #[test]
    fn surface_filter_accepts_all_and_a_single_surface() {
        for (raw, expected) in [
            ("all", CourtFilter::Any),
            ("synthetic", CourtFilter::Surface(CourtSurface::Synthetic)),
        ] {
            let cfg =
                Config::parse(&format!("surface_filter = \"{raw}\"\n{SAMPLE}")).expect("parse");
            assert_eq!(cfg.surface_filter(), expected);
        }
        assert!(validated(&format!("surface_filter = \"grass\"\n{SAMPLE}")).is_err());
    }

    #[test]
    fn validation_rejects_malformed_base_url() {
        assert!(validated(&SAMPLE.replace("https://kurse.zhs-muenchen.de", "not a url")).is_err());
    }

    #[test]
    fn split_ids_splits_trims_and_skips_empty_entries() {
        assert_eq!(
            split_ids(" 123 ,,456, "),
            HashSet::from(["123".to_string(), "456".to_string()])
        );
    }

    #[test]
    fn split_ids_empty_input_yields_no_admins() {
        assert!(split_ids("").is_empty());
    }
}
