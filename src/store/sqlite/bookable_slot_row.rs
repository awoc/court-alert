use chrono::{DateTime, Utc};
use rusqlite::Row;
use uuid::Uuid;

use crate::model::BookableSlot;

use super::DbRepr;

pub(super) struct BookableSlotRow {
    court_id: String,
    court_name: String,
    starts_at: String,
    ends_at: String,
    available_places: u32,
}

impl TryFrom<&Row<'_>> for BookableSlotRow {
    type Error = rusqlite::Error;

    fn try_from(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            court_id: row.get("court_id")?,
            court_name: row.get("court_name")?,
            starts_at: row.get("starts_at")?,
            ends_at: row.get("ends_at")?,
            available_places: row.get("available_places")?,
        })
    }
}

impl TryFrom<BookableSlotRow> for BookableSlot {
    type Error = rusqlite::Error;

    fn try_from(row: BookableSlotRow) -> rusqlite::Result<Self> {
        Ok(Self {
            court_id: Uuid::from_db(row.court_id)?,
            court_name: row.court_name,
            starts_at: DateTime::<Utc>::from_db(row.starts_at)?,
            ends_at: DateTime::<Utc>::from_db(row.ends_at)?,
            available_places: row.available_places,
        })
    }
}
