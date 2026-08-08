use chrono::{DateTime, Utc};
use rusqlite::Row;
use uuid::Uuid;

use crate::model::{AlertLine, AlertSurface};

use super::DbRepr;

pub(super) struct AlertMessageRow {
    pub(super) line_index: u32,
    club: Option<String>,
    court_id: String,
    court_name: String,
    starts_at: String,
    ends_at: String,
    struck: bool,
}

impl TryFrom<&Row<'_>> for AlertMessageRow {
    type Error = rusqlite::Error;

    fn try_from(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            line_index: row.get("line_index")?,
            club: row.get("club")?,
            court_id: row.get("court_id")?,
            court_name: row.get("court_name")?,
            starts_at: row.get("starts_at")?,
            ends_at: row.get("ends_at")?,
            struck: row.get("struck")?,
        })
    }
}

impl TryFrom<AlertMessageRow> for AlertLine {
    type Error = rusqlite::Error;

    fn try_from(row: AlertMessageRow) -> rusqlite::Result<Self> {
        Ok(Self {
            club: row.club,
            court_id: Uuid::from_db(row.court_id)?,
            court_name: row.court_name,
            starts_at: DateTime::<Utc>::from_db(row.starts_at)?,
            ends_at: DateTime::<Utc>::from_db(row.ends_at)?,
            struck: row.struck,
        })
    }
}

impl AlertSurface {
    pub(super) fn as_db(self) -> &'static str {
        match self {
            Self::Channel => "channel",
            Self::DirectMessage => "dm",
        }
    }
}
