use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::params;
use uuid::Uuid;

use crate::model::{BookableSlot, BookableSlotId, BookableSlotSnapshot, VenueId};
use crate::ports::{BookableSlotSnapshotRepository, VenueStateRepository};

use super::{BookableSlotRow, DbRepr, SqliteStore};

const SLOT_COLUMNS: &str = "venue_id, court_id, court_name, starts_at, ends_at, available_places";

fn map_slot(row: &rusqlite::Row<'_>) -> rusqlite::Result<(BookableSlotId, BookableSlot)> {
    let slot = BookableSlot::try_from(BookableSlotRow::try_from(row)?)?;
    Ok((BookableSlotId::from(&slot), slot))
}

#[async_trait]
impl BookableSlotSnapshotRepository for SqliteStore {
    async fn load_snapshot(&self) -> Result<BookableSlotSnapshot> {
        self.with_reader("load_bookable_slot_snapshot", |connection| {
            let mut statement = connection
                .prepare(&format!("SELECT {SLOT_COLUMNS} FROM bookable_slots"))
                .context("preparing bookable-slot snapshot load")?;
            let rows = statement
                .query_map([], map_slot)
                .context("querying bookable-slot snapshot")?;
            rows.collect::<rusqlite::Result<BookableSlotSnapshot>>()
                .context("collecting bookable-slot snapshot")
        })
        .await
    }

    async fn load_venue_snapshot(&self, venue_id: &VenueId) -> Result<BookableSlotSnapshot> {
        let venue_id = venue_id.to_string();
        self.with_reader("load_venue_bookable_slot_snapshot", move |connection| {
            let mut statement = connection
                .prepare(&format!(
                    "SELECT {SLOT_COLUMNS} FROM bookable_slots WHERE venue_id = ?1"
                ))
                .context("preparing venue bookable-slot snapshot load")?;
            let rows = statement
                .query_map(params![venue_id], map_slot)
                .context("querying venue bookable-slot snapshot")?;
            rows.collect::<rusqlite::Result<BookableSlotSnapshot>>()
                .context("collecting venue bookable-slot snapshot")
        })
        .await
    }

    async fn replace_venue_snapshot(
        &self,
        venue_id: &VenueId,
        slots: Vec<BookableSlot>,
    ) -> Result<()> {
        if let Some(foreign) = slots.iter().find(|slot| slot.venue_id != *venue_id) {
            anyhow::bail!(
                "slot for court {} belongs to venue {} but is being written under {venue_id}",
                foreign.court_id,
                foreign.venue_id
            );
        }
        let venue_id = venue_id.to_string();
        self.with_writer("replace_venue_bookable_slot_snapshot", move |connection| {
            let transaction = connection
                .transaction()
                .context("starting bookable-slot snapshot transaction")?;
            transaction
                .execute(
                    "DELETE FROM bookable_slots WHERE venue_id = ?1",
                    params![venue_id],
                )
                .context("clearing venue bookable-slot snapshot")?;
            {
                let mut statement = transaction
                    .prepare(&format!(
                        "INSERT INTO bookable_slots ({SLOT_COLUMNS})
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
                    ))
                    .context("preparing bookable-slot insert")?;
                for slot in slots {
                    statement
                        .execute(params![
                            slot.venue_id.to_string(),
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

    async fn delete_snapshots_except(&self, venue_ids: &[VenueId]) -> Result<u64> {
        let kept: Vec<String> = venue_ids.iter().map(VenueId::to_string).collect();
        self.with_writer("sweep_removed_venue_slots", move |connection| {
            let placeholders = (1..=kept.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = if kept.is_empty() {
                "DELETE FROM bookable_slots".to_string()
            } else {
                format!("DELETE FROM bookable_slots WHERE venue_id NOT IN ({placeholders})")
            };
            let affected = connection
                .execute(&sql, rusqlite::params_from_iter(kept.iter()))
                .context("sweeping slots of removed venues")?;
            Ok(affected as u64)
        })
        .await
    }
}

#[async_trait]
impl VenueStateRepository for SqliteStore {
    async fn is_initialised(&self, venue_id: &VenueId) -> Result<bool> {
        let venue_id = venue_id.to_string();
        self.with_reader("is_venue_initialised", move |connection| {
            connection
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM venue_state WHERE venue_id = ?1)",
                    params![venue_id],
                    |row| row.get(0),
                )
                .context("reading venue state")
        })
        .await
    }

    async fn delete_venue_state_except(&self, venue_ids: &[VenueId]) -> Result<u64> {
        let kept: Vec<String> = venue_ids.iter().map(VenueId::to_string).collect();
        self.with_writer("sweep_removed_venue_state", move |connection| {
            let placeholders = (1..=kept.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = if kept.is_empty() {
                "DELETE FROM venue_state".to_string()
            } else {
                format!("DELETE FROM venue_state WHERE venue_id NOT IN ({placeholders})")
            };
            let affected = connection
                .execute(&sql, rusqlite::params_from_iter(kept.iter()))
                .context("sweeping state of removed venues")?;
            Ok(affected as u64)
        })
        .await
    }

    async fn mark_initialised(&self, venue_id: &VenueId) -> Result<()> {
        let venue_id = venue_id.to_string();
        let now = Utc::now().into_db()?;
        self.with_writer("mark_venue_initialised", move |connection| {
            connection
                .execute(
                    "INSERT INTO venue_state (venue_id, initialised_at) VALUES (?1, ?2)
                     ON CONFLICT (venue_id) DO NOTHING",
                    params![venue_id, now],
                )
                .context("recording venue state")?;
            Ok(())
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

    /// Fixed-width UTC RFC 3339 with milliseconds, as the schema demands: it
    /// sorts and compares as text, unlike the variable "+00:00" offset form.
    fn into_db(self) -> Result<Self::Db> {
        Ok(self.to_rfc3339_opts(SecondsFormat::Millis, true))
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

    fn zhs() -> VenueId {
        VenueId::new("zhs-munich")
    }

    fn padel() -> VenueId {
        VenueId::new("casa-padel")
    }

    fn slot_at(venue_id: VenueId, court: &str, hour: u32) -> BookableSlot {
        let starts_at = Utc.with_ymd_and_hms(2026, 7, 13, hour, 0, 0).unwrap();
        BookableSlot {
            venue_id,
            court_id: Uuid::new_v4(),
            court_name: court.into(),
            starts_at,
            ends_at: starts_at + Duration::hours(1),
            available_places: 2,
        }
    }

    fn slot(court: &str, hour: u32) -> BookableSlot {
        slot_at(zhs(), court, hour)
    }

    #[tokio::test]
    async fn fresh_database_has_empty_snapshot() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        assert!(store.load_snapshot().await.unwrap().is_empty());
        assert!(store.load_venue_snapshot(&zhs()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn replacement_roundtrips_and_overwrites_the_venues_snapshot() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        store
            .replace_venue_snapshot(&zhs(), vec![slot("Court 1", 8)])
            .await
            .unwrap();

        let replacement = slot("Court 2", 9);
        store
            .replace_venue_snapshot(&zhs(), vec![replacement.clone()])
            .await
            .unwrap();

        let snapshot = store.load_snapshot().await.unwrap();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[&BookableSlotId::from(&replacement)], replacement);
    }

    #[tokio::test]
    async fn empty_replacement_clears_only_that_venue() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let kept = slot_at(padel(), "Court 3", 10);
        store
            .replace_venue_snapshot(&zhs(), vec![slot("Court 1", 8)])
            .await
            .unwrap();
        store
            .replace_venue_snapshot(&padel(), vec![kept.clone()])
            .await
            .unwrap();

        store
            .replace_venue_snapshot(&zhs(), Vec::new())
            .await
            .unwrap();

        assert!(store.load_venue_snapshot(&zhs()).await.unwrap().is_empty());
        assert_eq!(store.load_venue_snapshot(&padel()).await.unwrap().len(), 1);
        assert_eq!(store.load_snapshot().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_venue_snapshot_load_sees_only_its_own_slots() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        store
            .replace_venue_snapshot(&zhs(), vec![slot("Court 1", 8), slot("Court 2", 9)])
            .await
            .unwrap();
        store
            .replace_venue_snapshot(&padel(), vec![slot_at(padel(), "Court 1", 8)])
            .await
            .unwrap();

        let zhs_slots = store.load_venue_snapshot(&zhs()).await.unwrap();
        assert_eq!(zhs_slots.len(), 2);
        assert!(zhs_slots.values().all(|slot| slot.venue_id == zhs()));
        assert_eq!(store.load_snapshot().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn the_sweep_drops_only_venues_no_longer_configured() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        store
            .replace_venue_snapshot(&zhs(), vec![slot("Court 1", 8)])
            .await
            .unwrap();
        store
            .replace_venue_snapshot(&padel(), vec![slot_at(padel(), "Court 1", 8)])
            .await
            .unwrap();

        assert_eq!(store.delete_snapshots_except(&[zhs()]).await.unwrap(), 1);

        assert_eq!(store.load_venue_snapshot(&zhs()).await.unwrap().len(), 1);
        assert!(
            store
                .load_venue_snapshot(&padel())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_venue_is_uninitialised_until_it_is_marked() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        assert!(!store.is_initialised(&zhs()).await.unwrap());

        store.mark_initialised(&zhs()).await.unwrap();

        assert!(store.is_initialised(&zhs()).await.unwrap());
        assert!(!store.is_initialised(&padel()).await.unwrap());
    }

    #[tokio::test]
    async fn the_sweep_forgets_venues_no_longer_configured() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        store.mark_initialised(&zhs()).await.unwrap();
        store.mark_initialised(&padel()).await.unwrap();

        assert_eq!(store.delete_venue_state_except(&[zhs()]).await.unwrap(), 1);

        assert!(store.is_initialised(&zhs()).await.unwrap());
        assert!(
            !store.is_initialised(&padel()).await.unwrap(),
            "a removed venue must start quiet again if it comes back"
        );
    }

    #[tokio::test]
    async fn a_slot_from_another_venue_is_refused() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let foreign = slot_at(padel(), "Court 1", 8);

        let error = store
            .replace_venue_snapshot(&zhs(), vec![slot("Court 1", 8), foreign])
            .await
            .expect_err("a foreign slot must be refused");

        assert!(
            format!("{error:#}").contains("casa-padel"),
            "got: {error:#}"
        );
        assert!(
            store.load_snapshot().await.unwrap().is_empty(),
            "the transaction must not have written anything"
        );
    }

    #[tokio::test]
    async fn marking_a_venue_twice_is_harmless() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        store.mark_initialised(&zhs()).await.unwrap();
        store.mark_initialised(&zhs()).await.unwrap();
        assert!(store.is_initialised(&zhs()).await.unwrap());
    }
}
