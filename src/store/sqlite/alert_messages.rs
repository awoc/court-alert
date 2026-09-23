use std::collections::BTreeMap;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::params;

use crate::model::{
    AlertLine, AlertMessage, AlertMessageKey, AlertSurface, BookableSlotId, StrikePlan,
};
use crate::ports::AlertMessageRepository;

use super::alert_message_row::AlertDestination;
use super::{AlertMessageRow, DbRepr, SqliteStore};

#[async_trait]
impl AlertMessageRepository for SqliteStore {
    async fn record_message(&self, key: &AlertMessageKey, lines: &[AlertLine]) -> Result<()> {
        let key = key.clone();
        let lines = lines.to_vec();
        self.with_writer("record_alert_message", move |connection| {
            let destination = AlertDestination(key.destination.as_deref()).into_db()?;
            let transaction = connection
                .transaction()
                .context("starting alert-message insert transaction")?;
            {
                let mut statement = transaction
                    .prepare(
                        "INSERT INTO alert_message_slots
                         (chat_provider, surface, destination, message_id, line_index, club,
                          court_id, court_name, starts_at, ends_at, struck)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0)",
                    )
                    .context("preparing alert-message insert")?;
                for (index, line) in lines.into_iter().enumerate() {
                    statement
                        .execute(params![
                            key.chat_provider,
                            key.surface.as_db(),
                            destination,
                            key.id,
                            index as u32,
                            line.club,
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

    async fn plan_strikes(
        &self,
        chat_provider: &str,
        surface: AlertSurface,
        slots: &[BookableSlotId],
    ) -> Result<Vec<StrikePlan>> {
        let chat_provider = chat_provider.to_owned();
        let slots = slots.to_vec();
        self.with_reader("plan_alert_message_strikes", move |connection| {
            // Lookups and complete message loads must observe the same snapshot.
            let transaction = connection
                .transaction()
                .context("starting alert-message read transaction")?;
            let mut planned: BTreeMap<(String, String), Vec<u32>> = BTreeMap::new();
            {
                let mut find = transaction
                    .prepare(
                        "SELECT message_id, destination, line_index FROM alert_message_slots
                         WHERE chat_provider = ?1 AND surface = ?2
                           AND court_id = ?3 AND starts_at = ?4 AND struck = 0",
                    )
                    .context("preparing alert-message slot lookup")?;
                for slot in slots {
                    let rows = find
                        .query_map(
                            params![
                                chat_provider,
                                surface.as_db(),
                                slot.court_id.into_db()?,
                                slot.starts_at.into_db()?
                            ],
                            |row| {
                                Ok((
                                    row.get::<_, String>("message_id")?,
                                    row.get::<_, String>("destination")?,
                                    row.get("line_index")?,
                                ))
                            },
                        )
                        .context("querying alert-message slot lookup")?;
                    for row in rows {
                        let (message_id, destination, line_index) =
                            row.context("reading alert-message slot lookup")?;
                        planned
                            .entry((message_id, destination))
                            .or_default()
                            .push(line_index);
                    }
                }
            }
            let mut plans = Vec::with_capacity(planned.len());
            let mut load = transaction
                .prepare(
                    "SELECT line_index, club, court_id, court_name, starts_at, ends_at, struck
                     FROM alert_message_slots
                     WHERE chat_provider = ?1 AND surface = ?2 AND destination = ?3 AND message_id = ?4
                     ORDER BY line_index",
                )
                .context("preparing alert-message load")?;
            for ((message_id, destination), mut newly_struck) in planned {
                newly_struck.sort_unstable();
                newly_struck.dedup();
                let rows = load
                    .query_map(
                        params![chat_provider, surface.as_db(), destination, message_id],
                        |row| {
                            let row = AlertMessageRow::try_from(row)?;
                            let index = row.line_index;
                            AlertLine::try_from(row).map(|line| (index, line))
                        },
                    )
                    .context("querying alert-message load")?;
                let mut lines = Vec::new();
                for row in rows {
                    let (index, mut line) = row.context("reading alert-message line")?;
                    line.struck |= newly_struck.contains(&index);
                    lines.push(line);
                }
                plans.push(StrikePlan {
                    message: AlertMessage {
                        key: AlertMessageKey::new(
                            &chat_provider,
                            surface,
                            AlertDestination::from_db(&destination)?.0,
                            &message_id,
                        ),
                        lines,
                    },
                    newly_struck,
                });
            }
            Ok(plans)
        })
        .await
    }

    async fn commit_strikes(&self, key: &AlertMessageKey, lines: &[u32]) -> Result<()> {
        let key = key.clone();
        let lines = lines.to_vec();
        self.with_writer("commit_alert_message_strikes", move |connection| {
            let destination = AlertDestination(key.destination.as_deref()).into_db()?;
            let transaction = connection
                .transaction()
                .context("starting alert-message strike transaction")?;
            {
                let mut statement = transaction
                    .prepare(
                        "UPDATE alert_message_slots SET struck = 1
                         WHERE chat_provider = ?1 AND surface = ?2 AND destination = ?3
                           AND message_id = ?4 AND line_index = ?5",
                    )
                    .context("preparing alert-message strike")?;
                for line in lines {
                    statement
                        .execute(params![
                            key.chat_provider,
                            key.surface.as_db(),
                            destination,
                            key.id,
                            line
                        ])
                        .context("striking alert-message row")?;
                }
            }
            transaction
                .commit()
                .context("committing alert-message strikes")
        })
        .await
    }

    async fn forget_message(&self, key: &AlertMessageKey) -> Result<()> {
        let key = key.clone();
        self.with_writer("forget_alert_message", move |connection| {
            let destination = AlertDestination(key.destination.as_deref()).into_db()?;
            connection
                .execute(
                    "DELETE FROM alert_message_slots
                     WHERE chat_provider = ?1 AND surface = ?2 AND destination = ?3 AND message_id = ?4",
                    params![
                        key.chat_provider,
                        key.surface.as_db(),
                        destination,
                        key.id
                    ],
                )
                .context("deleting alert-message rows")?;
            Ok(())
        })
        .await
    }

    async fn prune_ended(&self, now: DateTime<Utc>) -> Result<usize> {
        let now = now.into_db()?;
        self.with_writer("prune_ended_alert_messages", move |connection| {
            let removed = connection
                .execute(
                    "DELETE FROM alert_message_slots
                     WHERE (chat_provider, surface, destination, message_id) IN (
                         SELECT chat_provider, surface, destination, message_id FROM alert_message_slots
                         GROUP BY chat_provider, surface, destination, message_id HAVING max(ends_at) <= ?1
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
    async fn record_message_line(
        &self,
        message_id: &str,
        line_index: u32,
        line: &AlertLine,
    ) -> Result<()> {
        let message_id = message_id.to_owned();
        let line = line.clone();
        self.with_writer("record_alert_message_line", move |connection| {
            connection
                .execute(
                    "INSERT INTO alert_message_slots
                     (chat_provider, surface, destination, message_id, line_index, club,
                      court_id, court_name, starts_at, ends_at, struck)
                     VALUES ('discord', 'channel', '', ?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
                    params![
                        message_id,
                        line_index,
                        line.club,
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
            club: None,
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

    async fn record(store: &SqliteStore, message_id: &str, lines: &[AlertLine]) {
        store
            .record_message(
                &AlertMessageKey::new("discord", AlertSurface::Channel, None, message_id),
                lines,
            )
            .await
            .unwrap();
    }

    async fn plan(store: &SqliteStore, slots: &[BookableSlotId]) -> Vec<StrikePlan> {
        store
            .plan_strikes("discord", AlertSurface::Channel, slots)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn recorded_lines_are_planned_and_committed() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let lines = vec![line("Court 1", 8), line("Court 2", 9)];
        record(&store, "1408", &lines).await;

        let plans = plan(&store, &[id_of(&lines[1])]).await;

        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].message.key.id, "1408");
        assert_eq!(plans[0].newly_struck, vec![1]);
        assert!(!plans[0].message.lines[0].struck, "untouched line");
        assert!(plans[0].message.lines[1].struck, "planned line");

        store
            .commit_strikes(
                &AlertMessageKey::new("discord", AlertSurface::Channel, None, "1408"),
                &[1],
            )
            .await
            .unwrap();

        let again = plan(&store, &[id_of(&lines[1])]).await;
        assert!(again.is_empty(), "a struck line is never planned again");
    }

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

        let plans = plan(&store, &[id_of(&lines[0])]).await;

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
        record(&store, "1408", std::slice::from_ref(&first)).await;
        record(&store, "1409", std::slice::from_ref(&first)).await;

        let plans = plan(&store, &[id_of(&first)]).await;

        let mut ids: Vec<_> = plans
            .iter()
            .map(|plan| plan.message.key.id.clone())
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["1408", "1409"], "the stale message self-heals");
    }

    #[tokio::test]
    async fn planning_an_unknown_slot_returns_nothing() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        record(&store, "1408", &[line("Court 1", 8)]).await;

        let plans = plan(&store, &[id_of(&line("Court 9", 20))]).await;

        assert!(plans.is_empty());
    }

    #[tokio::test]
    async fn a_slot_is_planned_only_on_the_surface_that_announced_it() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let announced = line("Court 1", 8);
        record(&store, "1408", std::slice::from_ref(&announced)).await;
        store
            .record_message(
                &AlertMessageKey::new("discord", AlertSurface::DirectMessage, Some("77"), "1409"),
                &[AlertLine {
                    club: Some("ZHS München".into()),
                    ..announced.clone()
                }],
            )
            .await
            .unwrap();

        let channel = plan(&store, &[id_of(&announced)]).await;
        let dms = store
            .plan_strikes("discord", AlertSurface::DirectMessage, &[id_of(&announced)])
            .await
            .unwrap();

        assert_eq!(channel.len(), 1);
        assert_eq!(channel[0].message.key.id, "1408");
        assert_eq!(channel[0].message.key.destination, None);
        assert_eq!(channel[0].message.lines[0].club, None);

        assert_eq!(dms.len(), 1);
        assert_eq!(dms[0].message.key.id, "1409");
        assert_eq!(
            dms[0].message.key.destination.as_deref(),
            Some("77"),
            "an edit needs the channel the DM lives in"
        );
        assert_eq!(dms[0].message.lines[0].club.as_deref(), Some("ZHS München"));
    }

    #[tokio::test]
    async fn every_dm_holding_a_slot_is_planned_with_its_own_channel() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let announced = AlertLine {
            club: Some("ZHS München".into()),
            ..line("Court 1", 8)
        };
        for (channel, message) in [("77", "1408"), ("78", "1409")] {
            store
                .record_message(
                    &AlertMessageKey::new(
                        "discord",
                        AlertSurface::DirectMessage,
                        Some(channel),
                        message,
                    ),
                    std::slice::from_ref(&announced),
                )
                .await
                .unwrap();
        }

        let plans = store
            .plan_strikes("discord", AlertSurface::DirectMessage, &[id_of(&announced)])
            .await
            .unwrap();

        let addressed: Vec<(String, Option<String>)> = plans
            .into_iter()
            .map(|plan| (plan.message.key.id, plan.message.key.destination))
            .collect();
        assert_eq!(
            addressed,
            vec![
                ("1408".to_string(), Some("77".to_string())),
                ("1409".to_string(), Some("78".to_string())),
            ]
        );
    }

    #[tokio::test]
    async fn forgetting_a_message_removes_every_line() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let lines = vec![line("Court 1", 8), line("Court 2", 9)];
        record(&store, "1408", &lines).await;
        record(&store, "1409", &[line("Court 3", 10)]).await;

        store
            .forget_message(&AlertMessageKey::new(
                "discord",
                AlertSurface::Channel,
                None,
                "1408",
            ))
            .await
            .unwrap();

        assert!(plan(&store, &[id_of(&lines[0])]).await.is_empty());
        assert!(plan(&store, &[id_of(&lines[1])]).await.is_empty());
        assert_eq!(
            store.prune_ended(far_future()).await.unwrap(),
            1,
            "1409 survived"
        );
    }

    fn far_future() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap()
    }

    #[tokio::test]
    async fn a_slot_that_has_started_but_not_ended_keeps_its_rows() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let only = line("Court 1", 8);
        record(&store, "1408", std::slice::from_ref(&only)).await;

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
        record(&store, "past", &[line("Court 1", 8), line("Court 2", 9)]).await;
        record(&store, "mixed", &[line("Court 3", 8), line("Court 4", 20)]).await;

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
    async fn pruning_covers_direct_messages_too() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let only = AlertLine {
            club: Some("ZHS München".into()),
            ..line("Court 1", 8)
        };
        store
            .record_message(
                &AlertMessageKey::new("discord", AlertSurface::DirectMessage, Some("77"), "1408"),
                std::slice::from_ref(&only),
            )
            .await
            .unwrap();

        assert_eq!(store.prune_ended(only.ends_at).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn pruning_an_empty_table_removes_nothing() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        assert_eq!(store.prune_ended(far_future()).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_slot_ending_exactly_now_counts_as_over() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let only = line("Court 1", 8);
        record(&store, "1408", std::slice::from_ref(&only)).await;

        let removed = store.prune_ended(only.ends_at).await.unwrap();

        assert_eq!(removed, 1);
    }

    async fn seed_reused_message_ids() -> (SqliteStore, AlertLine, [AlertMessageKey; 4]) {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let announced = line("Court 1", 8);
        let keys = [
            AlertMessageKey::new("first", AlertSurface::DirectMessage, Some("77"), "42"),
            AlertMessageKey::new("second", AlertSurface::DirectMessage, Some("77"), "42"),
            AlertMessageKey::new("second", AlertSurface::DirectMessage, Some("78"), "42"),
            AlertMessageKey::new("second", AlertSurface::Channel, Some("77"), "42"),
        ];
        for key in &keys {
            store
                .record_message(key, std::slice::from_ref(&announced))
                .await
                .unwrap();
        }
        (store, announced, keys)
    }

    async fn assert_pending_keys(
        store: &SqliteStore,
        announced: &AlertLine,
        seeded: &[AlertMessageKey],
        expected: &[AlertMessageKey],
    ) {
        let mut scopes = Vec::new();
        for key in seeded {
            let scope = (key.chat_provider.as_str(), key.surface);
            if !scopes.contains(&scope) {
                scopes.push(scope);
            }
        }
        for (chat_provider, surface) in scopes {
            let actual: Vec<_> = store
                .plan_strikes(chat_provider, surface, &[id_of(announced)])
                .await
                .unwrap()
                .into_iter()
                .map(|plan| plan.message.key)
                .collect();
            let expected: Vec<_> = expected
                .iter()
                .filter(|key| key.chat_provider == chat_provider && key.surface == surface)
                .cloned()
                .collect();
            assert_eq!(
                actual.len(),
                expected.len(),
                "{chat_provider} {surface:?}: {actual:?}"
            );
            for key in &expected {
                assert!(actual.contains(key), "missing {key:?} from {actual:?}");
            }
        }
    }

    #[tokio::test]
    async fn planning_isolates_chat_providers_and_surfaces_with_reused_message_ids() {
        let (store, announced, keys) = seed_reused_message_ids().await;
        assert_pending_keys(&store, &announced, &keys, &keys).await;
    }

    #[tokio::test]
    async fn committing_strikes_changes_only_the_complete_message_key() {
        let (store, announced, keys) = seed_reused_message_ids().await;
        store.commit_strikes(&keys[1], &[0]).await.unwrap();
        let expected: Vec<_> = keys
            .iter()
            .filter(|key| *key != &keys[1])
            .cloned()
            .collect();
        assert_pending_keys(&store, &announced, &keys, &expected).await;
    }

    #[tokio::test]
    async fn forgetting_changes_only_the_complete_message_key() {
        let (store, announced, keys) = seed_reused_message_ids().await;
        store.forget_message(&keys[1]).await.unwrap();
        let expected: Vec<_> = keys
            .iter()
            .filter(|key| *key != &keys[1])
            .cloned()
            .collect();
        assert_pending_keys(&store, &announced, &keys, &expected).await;
    }

    async fn seed_implicit_channel_message() -> (SqliteStore, AlertLine, AlertMessageKey) {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let announced = line("Court 1", 8);
        record(&store, "42", std::slice::from_ref(&announced)).await;
        let invalid = AlertMessageKey::new("discord", AlertSurface::Channel, Some(""), "42");
        (store, announced, invalid)
    }

    async fn assert_implicit_channel_message_unchanged(store: &SqliteStore, announced: &AlertLine) {
        let pending = plan(store, &[id_of(announced)]).await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].message.key.destination, None);
    }

    #[tokio::test]
    async fn an_empty_destination_cannot_commit_an_implicit_channel_message() {
        let (store, announced, invalid) = seed_implicit_channel_message().await;

        assert!(store.commit_strikes(&invalid, &[0]).await.is_err());
        assert_implicit_channel_message_unchanged(&store, &announced).await;
    }

    #[tokio::test]
    async fn an_empty_destination_cannot_forget_an_implicit_channel_message() {
        let (store, announced, invalid) = seed_implicit_channel_message().await;

        assert!(store.forget_message(&invalid).await.is_err());
        assert_implicit_channel_message_unchanged(&store, &announced).await;
    }

    #[tokio::test]
    async fn recording_an_empty_destination_is_rejected() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let announced = line("Court 1", 8);
        let invalid = AlertMessageKey::new("discord", AlertSurface::Channel, Some(""), "42");

        assert!(
            store
                .record_message(&invalid, std::slice::from_ref(&announced))
                .await
                .is_err()
        );
        assert!(plan(&store, &[id_of(&announced)]).await.is_empty());
    }

    #[tokio::test]
    async fn repeated_taken_slots_produce_one_strike_per_line() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let announced = line("Court 1", 8);
        record(&store, "42", std::slice::from_ref(&announced)).await;

        let plans = plan(&store, &[id_of(&announced), id_of(&announced)]).await;

        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].newly_struck, vec![0]);
        assert_eq!(plans[0].message.lines.len(), 1);
        assert!(plans[0].message.lines[0].struck);
    }

    #[tokio::test]
    async fn pruning_groups_by_the_full_message_key() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let early = line("Court 1", 8);
        let late = line("Court 2", 20);
        let keys = [
            AlertMessageKey::new("discord", AlertSurface::DirectMessage, Some("77"), "42"),
            AlertMessageKey::new("telegram", AlertSurface::DirectMessage, Some("77"), "42"),
            AlertMessageKey::new("telegram", AlertSurface::DirectMessage, Some("78"), "42"),
            AlertMessageKey::new("telegram", AlertSurface::Channel, Some("77"), "42"),
        ];
        for key in &keys[..3] {
            store
                .record_message(key, std::slice::from_ref(&early))
                .await
                .unwrap();
        }
        store
            .record_message(&keys[3], std::slice::from_ref(&late))
            .await
            .unwrap();
        assert_eq!(store.prune_ended(early.ends_at).await.unwrap(), 3);
        let plans = store
            .plan_strikes("telegram", AlertSurface::Channel, &[id_of(&late)])
            .await
            .unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].message.key, keys[3]);
        assert_eq!(store.prune_ended(late.ends_at).await.unwrap(), 1);
    }
}
