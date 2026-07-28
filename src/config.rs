use std::collections::HashSet;
use std::env::VarError;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use uuid::Uuid;

use crate::model::Court;

const MAX_LOOKAHEAD_DAYS: i64 = 366;
const DEFAULT_CONFIG_PATH: &str = "config.toml";
const DEFAULT_DB_PATH: &str = "data/court-alert.db";
const DEFAULT_WINDOW_START_HOUR: u32 = 8;
const DEFAULT_WINDOW_END_HOUR: u32 = 24;

#[derive(Debug, Clone)]
pub struct Config {
    poll_interval_secs: u64,
    lookahead_days: i64,
    base_url: String,
    quiet_first_poll: bool,
    operating_window_start_hour: u32,
    operating_window_end_hour: u32,
    courts: Vec<Court>,
}

#[derive(Deserialize)]
struct ConfigFile {
    poll_interval_secs: u64,
    lookahead_days: i64,
    base_url: String,
    #[serde(default = "default_true")]
    quiet_first_poll: bool,
    #[serde(default = "default_window_start_hour")]
    operating_window_start_hour: u32,
    #[serde(default = "default_window_end_hour")]
    operating_window_end_hour: u32,
    #[serde(rename = "products")]
    courts: Vec<CourtEntry>,
}

/// A court as the config file spells it. Kept separate from [`Court`] so the
/// model type carries no serde derive and cannot be constructed unvalidated:
/// entries are trimmed and checked in `validated_courts` on the way through.
#[derive(Deserialize)]
struct CourtEntry {
    id: Uuid,
    name: String,
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
    fn parse(raw: &str) -> Result<Self> {
        Self::from_file(toml::from_str(raw)?)
    }

    fn from_file(file: ConfigFile) -> Result<Self> {
        Self {
            poll_interval_secs: file.poll_interval_secs,
            lookahead_days: file.lookahead_days,
            base_url: file.base_url,
            quiet_first_poll: file.quiet_first_poll,
            operating_window_start_hour: file.operating_window_start_hour,
            operating_window_end_hour: file.operating_window_end_hour,
            courts: validated_courts(file.courts)?,
        }
        .validate()
    }

    fn validate(mut self) -> Result<Self> {
        anyhow::ensure!(
            self.poll_interval_secs > 0,
            "poll_interval_secs must be greater than zero"
        );
        anyhow::ensure!(
            (1..=MAX_LOOKAHEAD_DAYS).contains(&self.lookahead_days),
            "lookahead_days must be between 1 and {MAX_LOOKAHEAD_DAYS}"
        );
        anyhow::ensure!(
            self.operating_window_start_hour < self.operating_window_end_hour
                && self.operating_window_end_hour <= 24,
            "operating window invalid: start={}, end={} (require 0 <= start < end <= 24)",
            self.operating_window_start_hour,
            self.operating_window_end_hour
        );

        let url = reqwest::Url::parse(self.base_url.trim()).context("base_url is not a URL")?;
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
        self.base_url = url.as_str().trim_end_matches('/').to_string();

        Ok(self)
    }

    pub fn poll_interval_secs(&self) -> u64 {
        self.poll_interval_secs
    }

    pub fn lookahead_days(&self) -> i64 {
        self.lookahead_days
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn quiet_first_poll(&self) -> bool {
        self.quiet_first_poll
    }

    pub fn operating_window_start_hour(&self) -> u32 {
        self.operating_window_start_hour
    }

    pub fn operating_window_end_hour(&self) -> u32 {
        self.operating_window_end_hour
    }

    pub fn courts(&self) -> &[Court] {
        &self.courts
    }

    pub fn court_names(&self) -> Vec<String> {
        self.courts
            .iter()
            .map(|court| court.name().to_owned())
            .collect()
    }
}

/// Trims each name and rejects blanks and duplicate ids, then hands back model
/// types. This is the only place a [`Court`] is built, so every one in the
/// program has a non-blank name and a unique id.
fn validated_courts(entries: Vec<CourtEntry>) -> Result<Vec<Court>> {
    anyhow::ensure!(!entries.is_empty(), "products must not be empty");
    let mut ids = HashSet::with_capacity(entries.len());
    let mut courts = Vec::with_capacity(entries.len());
    for entry in entries {
        let name = entry.name.trim().to_string();
        anyhow::ensure!(!name.is_empty(), "product {} has a blank name", entry.id);
        anyhow::ensure!(ids.insert(entry.id), "duplicate product id {}", entry.id);
        courts.push(Court::new(entry.id, name));
    }
    Ok(courts)
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

    const SAMPLE: &str = r#"
poll_interval_secs = 300
lookahead_days = 7
base_url = "https://kurse.zhs-muenchen.de"

[[products]]
id = "92db7384-2dec-4888-a92a-4c2b6faac5f7"
name = "Tennis Court 1"

[[products]]
id = "11111111-2222-3333-4444-555555555555"
name = "Tennis Court 2"
"#;

    #[test]
    fn parses_config() {
        let cfg = Config::parse(SAMPLE).expect("parse");
        assert_eq!(cfg.poll_interval_secs(), 300);
        assert_eq!(cfg.lookahead_days(), 7);
        assert_eq!(cfg.base_url(), "https://kurse.zhs-muenchen.de");
        assert_eq!(cfg.courts().len(), 2);
        assert_eq!(cfg.courts()[0].name(), "Tennis Court 1");
        assert_eq!(
            cfg.courts()[0].id(),
            Uuid::parse_str("92db7384-2dec-4888-a92a-4c2b6faac5f7").unwrap()
        );
        assert_eq!(
            cfg.court_names(),
            vec!["Tennis Court 1".to_string(), "Tennis Court 2".to_string()]
        );
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
        assert_eq!(cfg.operating_window_start_hour(), 8);
        assert_eq!(cfg.operating_window_end_hour(), 24);
    }

    #[test]
    fn operating_window_must_be_within_a_single_day_and_nonempty() {
        let configured = SAMPLE.replace(
            "base_url = \"https://kurse.zhs-muenchen.de\"",
            "base_url = \"https://kurse.zhs-muenchen.de\"\noperating_window_start_hour = 6\noperating_window_end_hour = 22",
        );
        let cfg = Config::parse(&configured).expect("parse");
        assert_eq!(cfg.operating_window_start_hour(), 6);
        assert_eq!(cfg.operating_window_end_hour(), 22);

        assert!(Config::parse(&configured.replace("start_hour = 6", "start_hour = 22")).is_err());
        assert!(Config::parse(&configured.replace("end_hour = 22", "end_hour = 25")).is_err());
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
    fn validation_rejects_invalid_product_collections() {
        let empty = SAMPLE.split("[[products]]").next().unwrap();
        assert!(validated(empty).is_err());

        let duplicate = SAMPLE.replace(
            "11111111-2222-3333-4444-555555555555",
            "92db7384-2dec-4888-a92a-4c2b6faac5f7",
        );
        assert!(validated(&duplicate).is_err());

        assert!(validated(&SAMPLE.replace("Tennis Court 1", "   ")).is_err());
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
