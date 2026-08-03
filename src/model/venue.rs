use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VenueId(String);

impl VenueId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for VenueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for VenueId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl<'de> Deserialize<'de> for VenueId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self(String::deserialize(deserializer)?))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Sport {
    Tennis,
    Padel,
}

impl Sport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tennis => "tennis",
            Self::Padel => "padel",
        }
    }
}

impl fmt::Display for Sport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Sport {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> anyhow::Result<Self> {
        match raw.trim().to_lowercase().as_str() {
            "tennis" => Ok(Self::Tennis),
            "padel" => Ok(Self::Padel),
            other => anyhow::bail!("unknown sport {other:?} (expected tennis or padel)"),
        }
    }
}

impl<'de> Deserialize<'de> for Sport {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Zhs,
    Playtomic,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Zhs => "zhs",
            Self::Playtomic => "playtomic",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum VenueIdentity {
    Zhs { base_url: String },
    Playtomic { tenant_id: Uuid, slug: String },
}

impl VenueIdentity {
    pub fn provider(&self) -> Provider {
        match self {
            Self::Zhs { .. } => Provider::Zhs,
            Self::Playtomic { .. } => Provider::Playtomic,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Venue {
    pub id: VenueId,
    pub display_name: String,
    pub sport: Sport,
    pub identity: VenueIdentity,
    pub poll_interval_secs: Option<u64>,
    pub lookahead_days: Option<i64>,
    pub operating_window: Option<OperatingWindow>,
}

impl Venue {
    pub fn provider(&self) -> Provider {
        self.identity.provider()
    }
}

/// Half-open Berlin-local operating hours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperatingWindow {
    pub start_hour: u32,
    pub end_hour: u32,
}

impl OperatingWindow {
    pub fn new(start_hour: u32, end_hour: u32) -> anyhow::Result<Self> {
        anyhow::ensure!(
            start_hour < end_hour && end_hour <= 24,
            "operating window invalid: start={start_hour}, end={end_hour} \
             (require 0 <= start < end <= 24)"
        );
        Ok(Self {
            start_hour,
            end_hour,
        })
    }

    pub fn contains_hour(self, hour: u32) -> bool {
        (self.start_hour..self.end_hour).contains(&hour)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sports_parse_case_insensitively_and_round_trip() {
        assert_eq!("Tennis".parse::<Sport>().unwrap(), Sport::Tennis);
        assert_eq!(" PADEL ".parse::<Sport>().unwrap(), Sport::Padel);
        assert_eq!(Sport::Padel.to_string(), "padel");
        assert!("squash".parse::<Sport>().is_err());
    }

    #[test]
    fn an_identity_knows_its_provider() {
        let zhs = VenueIdentity::Zhs {
            base_url: "https://example.test".into(),
        };
        assert_eq!(zhs.provider(), Provider::Zhs);

        let playtomic = VenueIdentity::Playtomic {
            tenant_id: Uuid::nil(),
            slug: "a-club".into(),
        };
        assert_eq!(playtomic.provider(), Provider::Playtomic);
    }

    #[test]
    fn the_provider_tag_selects_the_identity_fields() {
        let zhs: VenueIdentity =
            toml::from_str("provider = \"zhs\"\nbase_url = \"https://example.test\"").unwrap();
        assert_eq!(
            zhs,
            VenueIdentity::Zhs {
                base_url: "https://example.test".into()
            }
        );

        assert!(
            toml::from_str::<VenueIdentity>(
                "provider = \"zhs\"\ntenant_id = \"00000000-0000-0000-0000-000000000000\"\nslug = \"x\""
            )
            .is_err()
        );
    }

    #[test]
    fn operating_windows_are_half_open_and_within_one_day() {
        let window = OperatingWindow::new(8, 24).unwrap();
        assert!(window.contains_hour(8));
        assert!(window.contains_hour(23));
        assert!(!window.contains_hour(7));
        assert!(!window.contains_hour(24));

        assert!(OperatingWindow::new(22, 22).is_err());
        assert!(OperatingWindow::new(8, 25).is_err());
    }
}
