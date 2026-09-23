use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::BookableSlot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertSurface {
    Channel,
    DirectMessage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertLine {
    pub club: Option<String>,
    pub court_id: Uuid,
    pub court_name: String,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub struck: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertMessageKey {
    pub chat_provider: String,
    pub surface: AlertSurface,
    /// Nonempty address within the chat provider. None is an implicit channel destination.
    pub destination: Option<String>,
    pub id: String,
}

impl AlertMessageKey {
    pub fn new(
        chat_provider: &str,
        surface: AlertSurface,
        destination: Option<&str>,
        id: &str,
    ) -> Self {
        Self {
            chat_provider: chat_provider.to_owned(),
            surface,
            destination: destination.map(str::to_owned),
            id: id.to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertMessage {
    pub key: AlertMessageKey,
    pub lines: Vec<AlertLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrikePlan {
    pub message: AlertMessage,
    pub newly_struck: Vec<u32>,
}

impl From<&BookableSlot> for AlertLine {
    fn from(slot: &BookableSlot) -> Self {
        Self {
            club: None,
            court_id: slot.court_id,
            court_name: slot.court_name.clone(),
            starts_at: slot.starts_at,
            ends_at: slot.ends_at,
            struck: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    #[test]
    fn a_line_built_from_a_slot_starts_unstruck() {
        let starts_at = Utc.with_ymd_and_hms(2026, 6, 2, 18, 0, 0).unwrap();
        let slot = BookableSlot {
            venue_id: crate::model::VenueId::new("zhs-munich"),
            court_id: Uuid::nil(),
            court_name: "Court 2".into(),
            starts_at,
            ends_at: starts_at + Duration::hours(1),
            available_places: 1,
        };

        let line = AlertLine::from(&slot);

        assert_eq!(line.court_id, slot.court_id);
        assert_eq!(line.court_name, "Court 2");
        assert_eq!(line.starts_at, starts_at);
        assert_eq!(line.ends_at, slot.ends_at);
        assert!(!line.struck, "a freshly announced line is never struck");
        assert_eq!(line.club, None, "a slot alone does not know its club");
    }
}
