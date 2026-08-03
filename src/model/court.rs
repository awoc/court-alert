use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer};
use uuid::Uuid;

use super::Sport;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum CourtSurface {
    #[default]
    Clay,
    Synthetic,
}

impl CourtSurface {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clay => "clay",
            Self::Synthetic => "synthetic",
        }
    }
}

impl fmt::Display for CourtSurface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CourtSurface {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> anyhow::Result<Self> {
        match raw.trim().to_lowercase().as_str() {
            "clay" => Ok(Self::Clay),
            "synthetic" => Ok(Self::Synthetic),
            other => anyhow::bail!("unknown court surface {other:?} (expected clay or synthetic)"),
        }
    }
}

impl<'de> Deserialize<'de> for CourtSurface {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CourtLocation {
    Indoor,
    Outdoor,
}

impl CourtLocation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Indoor => "indoor",
            Self::Outdoor => "outdoor",
        }
    }
}

impl fmt::Display for CourtLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CourtLocation {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> anyhow::Result<Self> {
        match raw.trim().to_lowercase().as_str() {
            "indoor" => Ok(Self::Indoor),
            "outdoor" => Ok(Self::Outdoor),
            other => {
                anyhow::bail!("unknown court location {other:?} (expected indoor or outdoor)")
            }
        }
    }
}

impl<'de> Deserialize<'de> for CourtLocation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CourtAttributes {
    Tennis { surface: CourtSurface },
    Padel { location: Option<CourtLocation> },
}

impl CourtAttributes {
    pub fn tennis(surface: CourtSurface) -> Self {
        Self::Tennis { surface }
    }

    pub fn padel(location: Option<CourtLocation>) -> Self {
        Self::Padel { location }
    }

    pub fn sport(&self) -> Sport {
        match self {
            Self::Tennis { .. } => Sport::Tennis,
            Self::Padel { .. } => Sport::Padel,
        }
    }

    pub fn surface(&self) -> Option<CourtSurface> {
        match self {
            Self::Tennis { surface } => Some(*surface),
            Self::Padel { .. } => None,
        }
    }

    pub fn location(&self) -> Option<CourtLocation> {
        match self {
            Self::Tennis { .. } => None,
            Self::Padel { location } => *location,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CourtFilter {
    Any,
    Surface(CourtSurface),
    Location(CourtLocation),
}

impl CourtFilter {
    pub const CLAY: Self = Self::Surface(CourtSurface::Clay);

    pub fn allows(self, attributes: Option<&CourtAttributes>) -> bool {
        match self {
            Self::Any => true,
            Self::Surface(wanted) => attributes.and_then(CourtAttributes::surface) == Some(wanted),
            Self::Location(wanted) => {
                attributes.and_then(CourtAttributes::location) == Some(wanted)
            }
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Surface(surface) => surface.as_str(),
            Self::Location(location) => location.as_str(),
        }
    }
}

impl fmt::Display for CourtFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CourtFilter {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> anyhow::Result<Self> {
        let trimmed = raw.trim();
        if trimmed.eq_ignore_ascii_case("any") || trimmed.eq_ignore_ascii_case("all") {
            return Ok(Self::Any);
        }
        if let Ok(surface) = trimmed.parse() {
            return Ok(Self::Surface(surface));
        }
        trimmed.parse().map(Self::Location).map_err(|_| {
            anyhow::anyhow!(
                "unknown court filter {raw:?} \
                 (expected any, clay, synthetic, indoor or outdoor)"
            )
        })
    }
}

impl<'de> Deserialize<'de> for CourtFilter {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Court {
    id: Uuid,
    name: String,
    attributes: CourtAttributes,
}

impl Court {
    pub fn new(id: Uuid, name: String, attributes: CourtAttributes) -> Self {
        Self {
            id,
            name,
            attributes,
        }
    }

    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn attributes(&self) -> &CourtAttributes {
        &self.attributes
    }

    pub fn number(&self) -> Option<u32> {
        first_number(&self.name)
    }
}

fn first_number(text: &str) -> Option<u32> {
    let start = text.find(|c: char| c.is_ascii_digit())?;
    text[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CourtCatalog {
    courts: Vec<Court>,
}

impl CourtCatalog {
    pub fn new(courts: Vec<Court>) -> Self {
        Self { courts }
    }

    pub fn courts(&self) -> &[Court] {
        &self.courts
    }

    pub fn names(&self) -> Vec<String> {
        self.courts
            .iter()
            .map(|court| court.name().to_owned())
            .collect()
    }

    pub fn attributes_of(&self, court_id: Uuid) -> Option<&CourtAttributes> {
        self.courts
            .iter()
            .find(|court| court.id() == court_id)
            .map(Court::attributes)
    }

    pub fn find_by_name(&self, name: &str) -> Option<&Court> {
        self.courts
            .iter()
            .find(|court| court.name().eq_ignore_ascii_case(name))
    }

    pub fn resolve(&self, input: &str) -> Option<&Court> {
        let input = input.trim();
        if let Some(number) = first_number(input)
            && let Some(court) = self
                .courts
                .iter()
                .find(|court| court.number() == Some(number))
        {
            return Some(court);
        }
        self.find_by_name(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> CourtCatalog {
        CourtCatalog::new(vec![
            Court::new(
                Uuid::from_u128(2),
                "Court 2".into(),
                CourtAttributes::tennis(CourtSurface::Clay),
            ),
            Court::new(
                Uuid::from_u128(19),
                "Court 19 - Synthetic".into(),
                CourtAttributes::tennis(CourtSurface::Synthetic),
            ),
            Court::new(
                Uuid::from_u128(99),
                "Centre Court".into(),
                CourtAttributes::tennis(CourtSurface::Clay),
            ),
        ])
    }

    #[test]
    fn surfaces_parse_case_insensitively_and_round_trip() {
        assert_eq!("Clay".parse::<CourtSurface>().unwrap(), CourtSurface::Clay);
        assert_eq!(
            " SYNTHETIC ".parse::<CourtSurface>().unwrap(),
            CourtSurface::Synthetic
        );
        assert_eq!(CourtSurface::Synthetic.to_string(), "synthetic");
        assert!("grass".parse::<CourtSurface>().is_err());
    }

    #[test]
    fn locations_parse_case_insensitively_and_round_trip() {
        assert_eq!(
            "Indoor".parse::<CourtLocation>().unwrap(),
            CourtLocation::Indoor
        );
        assert_eq!(
            " OUTDOOR ".parse::<CourtLocation>().unwrap(),
            CourtLocation::Outdoor
        );
        assert_eq!(CourtLocation::Indoor.to_string(), "indoor");
        assert!("roofed".parse::<CourtLocation>().is_err());
    }

    #[test]
    fn court_filters_parse_every_vocabulary_they_span() {
        assert_eq!("any".parse::<CourtFilter>().unwrap(), CourtFilter::Any);
        assert_eq!("all".parse::<CourtFilter>().unwrap(), CourtFilter::Any);
        assert_eq!("clay".parse::<CourtFilter>().unwrap(), CourtFilter::CLAY);
        assert_eq!(
            "synthetic".parse::<CourtFilter>().unwrap(),
            CourtFilter::Surface(CourtSurface::Synthetic)
        );
        assert_eq!(
            "indoor".parse::<CourtFilter>().unwrap(),
            CourtFilter::Location(CourtLocation::Indoor)
        );
        assert_eq!(
            "outdoor".parse::<CourtFilter>().unwrap(),
            CourtFilter::Location(CourtLocation::Outdoor)
        );
        assert!("grass".parse::<CourtFilter>().is_err());
    }

    #[test]
    fn a_filter_only_admits_its_own_attribute() {
        let clay = CourtAttributes::tennis(CourtSurface::Clay);
        let synthetic = CourtAttributes::tennis(CourtSurface::Synthetic);

        assert!(CourtFilter::Any.allows(Some(&synthetic)));
        assert!(CourtFilter::CLAY.allows(Some(&clay)));
        assert!(!CourtFilter::CLAY.allows(Some(&synthetic)));
    }

    #[test]
    fn a_location_filter_only_admits_its_own_location() {
        let indoor = CourtAttributes::padel(Some(CourtLocation::Indoor));
        let outdoor = CourtAttributes::padel(Some(CourtLocation::Outdoor));
        let indoor_filter = CourtFilter::Location(CourtLocation::Indoor);

        assert!(indoor_filter.allows(Some(&indoor)));
        assert!(!indoor_filter.allows(Some(&outdoor)));
        assert!(CourtFilter::Any.allows(Some(&outdoor)));
    }

    #[test]
    fn filters_and_courts_from_different_sports_never_match() {
        let padel = CourtAttributes::padel(Some(CourtLocation::Indoor));
        let tennis = CourtAttributes::tennis(CourtSurface::Clay);

        assert!(!CourtFilter::CLAY.allows(Some(&padel)));
        assert!(!CourtFilter::Location(CourtLocation::Indoor).allows(Some(&tennis)));
    }

    #[test]
    fn an_unknown_court_passes_only_the_any_filter() {
        assert!(CourtFilter::Any.allows(None));
        assert!(!CourtFilter::CLAY.allows(None));
        assert!(!CourtFilter::Location(CourtLocation::Indoor).allows(None));
    }

    #[test]
    fn a_padel_court_of_unknown_location_passes_only_the_any_filter() {
        let unknown = CourtAttributes::padel(None);

        assert!(CourtFilter::Any.allows(Some(&unknown)));
        assert!(!CourtFilter::Location(CourtLocation::Indoor).allows(Some(&unknown)));
        assert!(!CourtFilter::Location(CourtLocation::Outdoor).allows(Some(&unknown)));
    }

    #[test]
    fn attributes_report_the_sport_they_belong_to() {
        assert_eq!(
            CourtAttributes::tennis(CourtSurface::Clay).sport(),
            Sport::Tennis
        );
        assert_eq!(CourtAttributes::padel(None).sport(), Sport::Padel);
    }

    #[test]
    fn court_numbers_come_from_the_name() {
        let catalog = catalog();
        assert_eq!(catalog.courts()[1].number(), Some(19));
        assert_eq!(catalog.courts()[2].number(), None);
    }

    #[test]
    fn resolving_matches_on_the_court_number() {
        let catalog = catalog();
        for input in ["19", " 19 ", "Court 19", "court 19 - synthetic", "#19"] {
            assert_eq!(
                catalog.resolve(input).map(Court::name),
                Some("Court 19 - Synthetic"),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn resolving_falls_back_to_the_name_when_there_is_no_number() {
        let catalog = catalog();
        assert_eq!(
            catalog.resolve("centre court").map(Court::name),
            Some("Centre Court")
        );
        assert!(catalog.resolve("Court 7").is_none());
        assert!(catalog.resolve("nonsense").is_none());
    }

    #[test]
    fn attributes_are_looked_up_by_court_id() {
        let catalog = catalog();
        assert_eq!(
            catalog.attributes_of(Uuid::from_u128(19)),
            Some(&CourtAttributes::tennis(CourtSurface::Synthetic))
        );
        assert_eq!(
            catalog.attributes_of(Uuid::from_u128(2)),
            Some(&CourtAttributes::tennis(CourtSurface::Clay))
        );
        assert_eq!(catalog.attributes_of(Uuid::nil()), None);
    }
}
