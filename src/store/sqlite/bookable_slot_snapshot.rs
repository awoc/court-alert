use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::params;
use uuid::Uuid;

use crate::domain::{BookableSlot, BookableSlotId, BookableSlotSnapshot};
use crate::ports::BookableSlotSnapshotRepository;

use super::{BookableSlotRow, DbRepr, SqliteStore};

#[async_trait]
impl BookableSlotSnapshotRepository for SqliteStore {
    async fn load_snapshot(&self) -> Result<BookableSlotSnapshot> {
        self.with_conn("load_bookable_slot_snapshot", |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT product_id, product_name, start_at, end_at, availability
                     FROM slot_state",
                )
                .context("preparing bookable-slot snapshot load")?;
            let rows = statement
                .query_map([], |row| {
                    let row = BookableSlotRow::try_from(row)?;
                    BookableSlot::try_from(row)
                })
                .context("querying bookable-slot snapshot")?;
            rows.map(|result| result.map(|slot| (BookableSlotId::from(&slot), slot)))
                .collect::<rusqlite::Result<BookableSlotSnapshot>>()
                .context("collecting bookable-slot snapshot")
        })
        .await
    }

    async fn replace_snapshot(&self, slots: Vec<BookableSlot>) -> Result<()> {
        self.with_conn("replace_bookable_slot_snapshot", move |connection| {
            let transaction = connection
                .transaction()
                .context("starting bookable-slot snapshot transaction")?;
            transaction
                .execute("DELETE FROM slot_state", [])
                .context("clearing bookable-slot snapshot")?;
            {
                let mut statement = transaction
                    .prepare(
                        "INSERT INTO slot_state
                         (product_id, product_name, start_at, end_at, availability)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                    )
                    .context("preparing bookable-slot insert")?;
                for slot in slots {
                    statement
                        .execute(params![
                            slot.court_id.into_db()?,
                            slot.court_name,
                            slot.starts_at.into_db()?,
                            slot.ends_at.into_db()?,
                            slot.available_places,
                        ])
                        .context("inserting bookable-slot row")?;
                }
            }
            transaction
                .commit()
                .context("committing bookable-slot snapshot")
        })
        .await
    }
}

impl DbRepr for Uuid {
    type Db = String;

    fn into_db(self) -> Result<Self::Db> {
        Ok(self.to_string())
    }

    fn from_db(db: Self::Db) -> rusqlite::Result<Self> {
        Uuid::parse_str(&db).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    }
}

impl DbRepr for DateTime<Utc> {
    type Db = String;

    fn into_db(self) -> Result<Self::Db> {
        Ok(self.to_rfc3339())
    }

    fn from_db(db: Self::Db) -> rusqlite::Result<Self> {
        DateTime::parse_from_rfc3339(&db)
            .map(|datetime| datetime.with_timezone(&Utc))
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn slot(court: &str, hour: u32) -> BookableSlot {
        let starts_at = Utc.with_ymd_and_hms(2026, 7, 13, hour, 0, 0).unwrap();
        BookableSlot {
            court_id: Uuid::new_v4(),
            court_name: court.into(),
            starts_at,
            ends_at: starts_at + Duration::hours(1),
            available_places: 2,
        }
    }

    #[tokio::test]
    async fn fresh_database_has_empty_snapshot() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        assert!(store.load_snapshot().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn replacement_roundtrips_and_overwrites_snapshot() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let first = slot("Court 1", 8);
        store.replace_snapshot(vec![first]).await.unwrap();

        let replacement = slot("Court 2", 9);
        store
            .replace_snapshot(vec![replacement.clone()])
            .await
            .unwrap();

        let snapshot = store.load_snapshot().await.unwrap();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[&BookableSlotId::from(&replacement)], replacement);
    }

    #[tokio::test]
    async fn empty_replacement_clears_snapshot() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        store
            .replace_snapshot(vec![slot("Court 1", 8)])
            .await
            .unwrap();
        store.replace_snapshot(Vec::new()).await.unwrap();
        assert!(store.load_snapshot().await.unwrap().is_empty());
    }
}
