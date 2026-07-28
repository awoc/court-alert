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

    async fn forget_message(&self, message_id: &str) -> Result<()> {
        let message_id = message_id.to_owned();
        self.with_conn("forget_alert_message", move |connection| {
            connection
                .execute(
                    "DELETE FROM alert_message_slots WHERE message_id = ?1",
                    params![message_id],
                )
                .context("deleting alert-message rows")?;
            Ok(())
        })
        .await
    }

    async fn prune_ended(&self, now: DateTime<Utc>) -> Result<usize> {
        let now = now.into_db()?;
        self.with_conn("prune_ended_alert_messages", move |connection| {
            let removed = connection
                .execute(
                    "DELETE FROM alert_message_slots WHERE message_id IN (
                         SELECT message_id FROM alert_message_slots
                         GROUP BY message_id HAVING max(ends_at) <= ?1
                     )",
                    params![now],
                )
                .context("pruning ended alert messages")?;
            Ok(removed)
        })
        .await
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
    /// returned order. Insert the rows in reverse to rule out an
    /// insertion-order coincidence. Note that the schema's `WITHOUT ROWID`
    /// clustering on `(message_id, line_index)` means this test alone cannot
    /// detect a dropped `ORDER BY` — that contract is pinned by the port's
    /// documentation, not by this test.
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

    #[tokio::test]
    async fn forgetting_a_message_removes_every_line() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let lines = vec![line("Court 1", 8), line("Court 2", 9)];
        store.record_message("1408", &lines).await.unwrap();
        store
            .record_message("1409", &[line("Court 3", 10)])
            .await
            .unwrap();

        store.forget_message("1408").await.unwrap();

        assert!(
            store
                .plan_strikes(&[id_of(&lines[0])])
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .plan_strikes(&[id_of(&lines[1])])
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.prune_ended(far_future()).await.unwrap(),
            1,
            "1409 survived"
        );
    }

    fn far_future() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap()
    }

    /// The retention rule. A slot with no booking deadline stays bookable after
    /// its own start time, so its removal can arrive mid-slot and must still
    /// find a row to strike. Past `ends_at` it cannot be bookable at all, which
    /// is the first moment nothing about the message can change again.
    #[tokio::test]
    async fn a_slot_that_has_started_but_not_ended_keeps_its_rows() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let only = line("Court 1", 8);
        store
            .record_message("1408", std::slice::from_ref(&only))
            .await
            .unwrap();

        let mid_slot = only.starts_at + Duration::minutes(20);
        assert_eq!(
            store.prune_ended(mid_slot).await.unwrap(),
            0,
            "the slot is still bookable, so its removal is still strikeable"
        );
        assert_eq!(store.prune_ended(only.ends_at).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn pruning_drops_only_messages_whose_slots_have_all_ended() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        // Slots 08:00–09:00 and 09:00–10:00 on 2026-07-13.
        store
            .record_message("past", &[line("Court 1", 8), line("Court 2", 9)])
            .await
            .unwrap();
        // One over, one still to come — the message must survive.
        store
            .record_message("mixed", &[line("Court 3", 8), line("Court 4", 20)])
            .await
            .unwrap();

        // 10:00 on the same day: "past" is fully over, "mixed" is not.
        let now = Utc.with_ymd_and_hms(2026, 7, 13, 10, 0, 0).unwrap();
        let removed = store.prune_ended(now).await.unwrap();

        assert_eq!(removed, 2, "both lines of `past`, none of `mixed`");
        assert_eq!(
            store.prune_ended(far_future()).await.unwrap(),
            2,
            "`mixed` is pruned once its later slot is over too"
        );
    }

    #[tokio::test]
    async fn pruning_an_empty_table_removes_nothing() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        assert_eq!(store.prune_ended(far_future()).await.unwrap(), 0);
    }

    /// A slot ending exactly at `now` is over. With a `<` comparison it would
    /// survive, and since pruning runs once a day, survive until tomorrow.
    #[tokio::test]
    async fn a_slot_ending_exactly_now_counts_as_over() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let only = line("Court 1", 8);
        store
            .record_message("1408", std::slice::from_ref(&only))
            .await
            .unwrap();

        let removed = store.prune_ended(only.ends_at).await.unwrap();

        assert_eq!(removed, 1);
    }
}
