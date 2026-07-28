use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer};
use uuid::Uuid;

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
pub enum SurfaceFilter {
    All,
    Only(CourtSurface),
}

impl SurfaceFilter {
    pub const CLAY: Self = Self::Only(CourtSurface::Clay);

    pub fn allows(self, surface: Option<CourtSurface>) -> bool {
        match self {
            Self::All => true,
            Self::Only(wanted) => surface == Some(wanted),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Only(surface) => surface.as_str(),
        }
    }
}

impl Default for SurfaceFilter {
    fn default() -> Self {
        Self::CLAY
    }
}

impl fmt::Display for SurfaceFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SurfaceFilter {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> anyhow::Result<Self> {
        if raw.trim().eq_ignore_ascii_case("all") {
            return Ok(Self::All);
        }
        raw.parse().map(Self::Only).map_err(|_| {
            anyhow::anyhow!("unknown surface {raw:?} (expected all, clay or synthetic)")
        })
    }
}

impl<'de> Deserialize<'de> for SurfaceFilter {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone)]
pub struct Court {
    id: Uuid,
    name: String,
    surface: CourtSurface,
}

impl Court {
    pub fn new(id: Uuid, name: String, surface: CourtSurface) -> Self {
        Self { id, name, surface }
    }

    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn surface(&self) -> CourtSurface {
        self.surface
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

#[derive(Debug, Clone, Default)]
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

    pub fn surface_of(&self, court_id: Uuid) -> Option<CourtSurface> {
        self.courts
            .iter()
            .find(|court| court.id() == court_id)
            .map(Court::surface)
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
            Court::new(Uuid::from_u128(2), "Court 2".into(), CourtSurface::Clay),
            Court::new(
                Uuid::from_u128(19),
                "Court 19 - Synthetic".into(),
                CourtSurface::Synthetic,
            ),
            Court::new(
                Uuid::from_u128(99),
                "Centre Court".into(),
                CourtSurface::Clay,
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
    fn surface_filters_parse_all_and_single_surfaces() {
        assert_eq!("all".parse::<SurfaceFilter>().unwrap(), SurfaceFilter::All);
        assert_eq!(
            "clay".parse::<SurfaceFilter>().unwrap(),
            SurfaceFilter::CLAY
        );
        assert_eq!(
            "synthetic".parse::<SurfaceFilter>().unwrap(),
            SurfaceFilter::Only(CourtSurface::Synthetic)
        );
        assert!("grass".parse::<SurfaceFilter>().is_err());
        assert_eq!(SurfaceFilter::default(), SurfaceFilter::CLAY);
    }

    #[test]
    fn a_filter_only_admits_its_own_surface() {
        assert!(SurfaceFilter::All.allows(Some(CourtSurface::Synthetic)));
        assert!(SurfaceFilter::CLAY.allows(Some(CourtSurface::Clay)));
        assert!(!SurfaceFilter::CLAY.allows(Some(CourtSurface::Synthetic)));
    }

    #[test]
    fn an_unknown_surface_passes_only_the_all_filter() {
        assert!(SurfaceFilter::All.allows(None));
        assert!(!SurfaceFilter::CLAY.allows(None));
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
    fn surfaces_are_looked_up_by_court_id() {
        let catalog = catalog();
        assert_eq!(
            catalog.surface_of(Uuid::from_u128(19)),
            Some(CourtSurface::Synthetic)
        );
        assert_eq!(
            catalog.surface_of(Uuid::from_u128(2)),
            Some(CourtSurface::Clay)
        );
        assert_eq!(catalog.surface_of(Uuid::nil()), None);
    }
}
