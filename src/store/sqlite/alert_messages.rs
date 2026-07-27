use std::collections::BTreeMap;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::params;

use crate::domain::{AlertLine, AlertMessage, BookableSlotId, StrikePlan};
use crate::ports::AlertMessageRepository;

use super::{AlertMessageRow, DbRepr, SqliteStore};

#[async_trait]
impl AlertMessageRepository for SqliteStore {
    async fn record_message(&self, message_id: &str, lines: &[AlertLine]) -> Result<()> {
        let message_id = message_id.to_owned();
        let lines = lines.to_vec();
        self.with_conn("record_alert_message", move |connection| {
            let transaction = connection
                .transaction()
                .context("starting alert-message insert transaction")?;
            {
                let mut statement = transaction
                    .prepare(
                        "INSERT INTO alert_message_slots
                         (message_id, line_index, court_id, court_name, starts_at, ends_at, struck)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                    )
                    .context("preparing alert-message insert")?;
                for (index, line) in lines.into_iter().enumerate() {
                    statement
                        .execute(params![
                            message_id,
                            index as u32,
                            line.court_id.into_db()?,
                            line.court_name,
                            line.starts_at.into_db()?,
                            line.ends_at.into_db()?,
                        ])
                        .context("inserting alert-message row")?;
                }
            }
            transaction
                .commit()
                .context("committing alert-message insert")
        })
        .await
    }

    async fn plan_strikes(&self, slots: &[BookableSlotId]) -> Result<Vec<StrikePlan>> {
        let slots = slots.to_vec();
        self.with_conn("plan_alert_message_strikes", move |connection| {
            // Which message holds each slot, and at which line. A slot can be
            // unstruck in several messages; every one of them is repaired.
            let mut planned: BTreeMap<String, Vec<u32>> = BTreeMap::new();
            {
                let mut find = connection
                    .prepare(
                        "SELECT message_id, line_index FROM alert_message_slots
                         WHERE court_id = ?1 AND starts_at = ?2 AND struck = 0",
                    )
                    .context("preparing alert-message slot lookup")?;
                for slot in slots {
                    let rows = find
                        .query_map(
                            params![slot.court_id.into_db()?, slot.starts_at.into_db()?],
                            |row| Ok((row.get::<_, String>("message_id")?, row.get("line_index")?)),
                        )
                        .context("querying alert-message slot lookup")?;
                    for row in rows {
                        let (message_id, line_index) =
                            row.context("reading alert-message slot lookup")?;
                        planned.entry(message_id).or_default().push(line_index);
                    }
                }
            }

            let mut plans = Vec::with_capacity(planned.len());
            let mut load = connection
                .prepare(
                    "SELECT line_index, court_id, court_name, starts_at, ends_at, struck
                     FROM alert_message_slots WHERE message_id = ?1 ORDER BY line_index",
                )
                .context("preparing alert-message load")?;
            for (message_id, mut newly_struck) in planned {
                newly_struck.sort_unstable();
                let rows = load
                    .query_map(params![message_id], |row| {
                        let row = AlertMessageRow::try_from(row)?;
                        let index = row.line_index;
                        AlertLine::try_from(row).map(|line| (index, line))
                    })
                    .context("querying alert-message load")?;
                let mut lines = Vec::new();
                for row in rows {
                    let (index, mut line) = row.context("reading alert-message line")?;
                    line.struck |= newly_struck.contains(&index);
                    lines.push(line);
                }
                plans.push(StrikePlan {
                    message: AlertMessage {
                        id: message_id,
                        lines,
                    },
                    newly_struck,
                });
            }
            Ok(plans)
        })
        .await
    }

    async fn commit_strikes(&self, message_id: &str, lines: &[u32]) -> Result<()> {
        let message_id = message_id.to_owned();
        let lines = lines.to_vec();
        self.with_conn("commit_alert_message_strikes", move |connection| {
            let transaction = connection
                .transaction()
                .context("starting alert-message strike transaction")?;
            {
                let mut statement = transaction
                    .prepare(
                        "UPDATE alert_message_slots SET struck = 1
                         WHERE message_id = ?1 AND line_index = ?2",
                    )
                    .context("preparing alert-message strike")?;
                for line in lines {
                    statement
                        .execute(params![message_id, line])
                        .context("striking alert-message row")?;
                }
            }
            transaction
                .commit()
                .context("committing alert-message strikes")
        })
        .await
    }

    async fn forget_message(&self, _message_id: &str) -> Result<()> {
        todo!()
    }

    async fn prune_started(&self, _now: DateTime<Utc>) -> Result<usize> {
        todo!()
    }
}

#[cfg(test)]
impl SqliteStore {
    /// Inserts a single line at an explicit index, so tests can write rows in
    /// an order other than ascending `line_index`.
    async fn record_message_line(
        &self,
        message_id: &str,
        line_index: u32,
        line: &AlertLine,
    ) -> Result<()> {
        let message_id = message_id.to_owned();
        let line = line.clone();
        self.with_conn("record_alert_message_line", move |connection| {
            connection
                .execute(
                    "INSERT INTO alert_message_slots
                     (message_id, line_index, court_id, court_name, starts_at, ends_at, struck)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                    params![
                        message_id,
                        line_index,
                        line.court_id.into_db()?,
                        line.court_name,
                        line.starts_at.into_db()?,
                        line.ends_at.into_db()?,
                    ],
                )
                .context("inserting alert-message row")?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};
    use uuid::Uuid;

    fn line(court: &str, hour: u32) -> AlertLine {
        let starts_at = Utc.with_ymd_and_hms(2026, 7, 13, hour, 0, 0).unwrap();
        AlertLine {
            court_id: Uuid::new_v4(),
            court_name: court.into(),
            starts_at,
            ends_at: starts_at + Duration::hours(1),
            struck: false,
        }
    }

    fn id_of(line: &AlertLine) -> BookableSlotId {
        BookableSlotId {
            court_id: line.court_id,
            starts_at: line.starts_at,
        }
    }

    #[tokio::test]
    async fn recorded_lines_are_planned_and_committed() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let lines = vec![line("Court 1", 8), line("Court 2", 9)];
        store.record_message("1408", &lines).await.unwrap();

        let plans = store.plan_strikes(&[id_of(&lines[1])]).await.unwrap();

        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].message.id, "1408");
        assert_eq!(plans[0].newly_struck, vec![1]);
        assert!(!plans[0].message.lines[0].struck, "untouched line");
        assert!(plans[0].message.lines[1].struck, "planned line");

        store.commit_strikes("1408", &[1]).await.unwrap();

        let again = store.plan_strikes(&[id_of(&lines[1])]).await.unwrap();
        assert!(again.is_empty(), "a struck line is never planned again");
    }

    /// `AlertLine` carries no index, so rendering depends entirely on the
    /// returned order. Insert the rows in reverse so that returning them in
    /// insertion order would fail — the ordering cannot pass by luck.
    #[tokio::test]
    async fn planned_lines_come_back_in_line_index_order() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let lines = [line("Court 1", 8), line("Court 2", 9), line("Court 3", 10)];
        for (index, line) in lines.iter().enumerate().rev() {
            store
                .record_message_line("1408", index as u32, line)
                .await
                .unwrap();
        }

        let plans = store.plan_strikes(&[id_of(&lines[0])]).await.unwrap();

        let names: Vec<_> = plans[0]
            .message
            .lines
            .iter()
            .map(|line| line.court_name.as_str())
            .collect();
        assert_eq!(names, vec!["Court 1", "Court 2", "Court 3"]);
    }

    #[tokio::test]
    async fn a_slot_unstruck_in_two_messages_is_planned_in_both() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let first = line("Court 1", 8);
        // `std::slice::from_ref`, not `&[first.clone()]` — clippy's
        // `cloned_ref_to_slice_refs` is on by default and `-D warnings` is a gate.
        store
            .record_message("1408", std::slice::from_ref(&first))
            .await
            .unwrap();
        store
            .record_message("1409", std::slice::from_ref(&first))
            .await
            .unwrap();

        let plans = store.plan_strikes(&[id_of(&first)]).await.unwrap();

        let mut ids: Vec<_> = plans.iter().map(|plan| plan.message.id.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["1408", "1409"], "the stale message self-heals");
    }

    #[tokio::test]
    async fn planning_an_unknown_slot_returns_nothing() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        store
            .record_message("1408", &[line("Court 1", 8)])
            .await
            .unwrap();

        let plans = store
            .plan_strikes(&[id_of(&line("Court 9", 20))])
            .await
            .unwrap();

        assert!(plans.is_empty());
    }
}
