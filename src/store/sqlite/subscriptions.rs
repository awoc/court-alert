use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::NaiveDate;
use rusqlite::{Row, params};

use crate::model::{ProviderUserRef, Subscription, SubscriptionDraft};
use crate::ports::SubscriptionRepository;

use super::{DbRepr, SqliteStore, SubscriptionRow};

const SUB_COLUMNS: &str =
    "id, provider, user_id, weekday, on_date, start_minute, end_minute, courts";

fn map_subscription(row: &Row<'_>) -> rusqlite::Result<Subscription> {
    Subscription::try_from(SubscriptionRow::try_from(row)?)
}

#[async_trait]
impl SubscriptionRepository for SqliteStore {
    async fn add(&self, sub: SubscriptionDraft) -> Result<i64> {
        self.with_conn("insert", move |conn| {
            let (weekday, on_date) = sub.schedule.into_db()?;
            let courts_json = sub.courts.into_db()?;
            conn.execute(
                "INSERT INTO subscriptions
                 (provider, user_id, weekday, on_date, start_minute, end_minute, courts)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    sub.user.provider,
                    sub.user.user_id,
                    weekday,
                    on_date,
                    sub.time_range.start_minute(),
                    sub.time_range.end_minute(),
                    courts_json,
                ],
            )
            .context("inserting subscription")?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    async fn list_for_user(
        &self,
        user: &ProviderUserRef,
        today: NaiveDate,
    ) -> Result<Vec<Subscription>> {
        let user = user.clone();
        let today = today.format("%Y-%m-%d").to_string();
        self.with_conn("list_for_user", move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {SUB_COLUMNS}
                     FROM subscriptions
                     WHERE provider = ?1 AND user_id = ?2
                       AND (on_date IS NULL OR on_date >= ?3)
                     ORDER BY id"
                ))
                .context("preparing list_for_user")?;
            let rows = stmt
                .query_map(
                    params![user.provider, user.user_id, today],
                    map_subscription,
                )
                .context("querying subscriptions")?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .context("collecting subscriptions")
        })
        .await
    }

    async fn list_all(&self, today: NaiveDate) -> Result<Vec<Subscription>> {
        let today = today.format("%Y-%m-%d").to_string();
        self.with_conn("list_all", move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {SUB_COLUMNS}
                     FROM subscriptions
                     WHERE on_date IS NULL OR on_date >= ?1
                     ORDER BY id"
                ))
                .context("preparing list_all")?;
            let rows = stmt
                .query_map(params![today], map_subscription)
                .context("querying subscriptions")?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .context("collecting subscriptions")
        })
        .await
    }

    async fn remove(&self, id: i64, user: &ProviderUserRef) -> Result<bool> {
        let user = user.clone();
        self.with_conn("remove", move |conn| {
            let affected = conn
                .execute(
                    "DELETE FROM subscriptions
                     WHERE id = ?1 AND provider = ?2 AND user_id = ?3",
                    params![id, user.provider, user.user_id],
                )
                .context("deleting subscription")?;
            Ok(affected > 0)
        })
        .await
    }

    async fn remove_any(&self, id: i64) -> Result<bool> {
        self.with_conn("remove_any", move |conn| {
            let affected = conn
                .execute("DELETE FROM subscriptions WHERE id = ?1", params![id])
                .context("deleting subscription by id")?;
            Ok(affected > 0)
        })
        .await
    }

    async fn remove_expired(&self, today: chrono::NaiveDate) -> Result<u64> {
        let today = today.format("%Y-%m-%d").to_string();
        self.with_conn("remove_expired", move |conn| {
            let affected = conn
                .execute(
                    "DELETE FROM subscriptions WHERE on_date IS NOT NULL AND on_date < ?1",
                    params![today],
                )
                .context("deleting expired subscriptions")?;
            Ok(affected as u64)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Schedule, TimeRange};
    use chrono::{NaiveDate, Weekday};

    fn uref(id: &str) -> ProviderUserRef {
        ProviderUserRef {
            provider: "discord".into(),
            user_id: id.into(),
        }
    }

    fn query_date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2000, 1, 1).unwrap()
    }

    fn range(from: u32, to: u32) -> TimeRange {
        TimeRange::new(from, to).unwrap()
    }

    fn sample(weekday: Weekday) -> SubscriptionDraft {
        SubscriptionDraft {
            user: uref("12345"),
            schedule: Schedule::Weekday(weekday),
            time_range: range(18 * 60, 22 * 60),
            courts: None,
        }
    }

    #[tokio::test]
    async fn add_then_list_for_user_returns_inserted_row() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let id = store.add(sample(Weekday::Tue)).await.unwrap();
        let subs = store
            .list_for_user(&uref("12345"), query_date())
            .await
            .unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].id, id);
        assert_eq!(subs[0].user, uref("12345"));
        assert_eq!(subs[0].schedule, Schedule::Weekday(Weekday::Tue));
        assert_eq!(subs[0].time_range, range(1080, 1320));
        assert!(subs[0].courts.is_none());
    }

    #[tokio::test]
    async fn list_for_user_filters_by_user_and_provider() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        store.add(sample(Weekday::Tue)).await.unwrap();
        let mut other_user = sample(Weekday::Wed);
        other_user.user = uref("99999");
        store.add(other_user).await.unwrap();
        let mut other_provider = sample(Weekday::Thu);
        other_provider.user = ProviderUserRef {
            provider: "telegram".into(),
            user_id: "12345".into(),
        };
        store.add(other_provider).await.unwrap();

        let subs = store
            .list_for_user(&uref("12345"), query_date())
            .await
            .unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].schedule, Schedule::Weekday(Weekday::Tue));
    }

    #[tokio::test]
    async fn list_all_returns_everything() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        store.add(sample(Weekday::Tue)).await.unwrap();
        let mut other = sample(Weekday::Wed);
        other.user = uref("99999");
        store.add(other).await.unwrap();
        assert_eq!(store.list_all(query_date()).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn remove_only_succeeds_for_owner() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let id = store.add(sample(Weekday::Tue)).await.unwrap();
        assert!(!store.remove(id, &uref("99999")).await.unwrap());
        let foreign = ProviderUserRef {
            provider: "telegram".into(),
            user_id: "12345".into(),
        };
        assert!(!store.remove(id, &foreign).await.unwrap());
        assert!(store.remove(id, &uref("12345")).await.unwrap());
        assert_eq!(
            store
                .list_for_user(&uref("12345"), query_date())
                .await
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn courts_filter_roundtrip() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let mut sub = sample(Weekday::Tue);
        sub.courts = Some(vec!["Court 2".into(), "Court 5".into()]);
        store.add(sub).await.unwrap();
        let subs = store
            .list_for_user(&uref("12345"), query_date())
            .await
            .unwrap();
        assert_eq!(
            subs[0].courts.as_deref(),
            Some(&["Court 2".to_string(), "Court 5".to_string()][..])
        );
    }

    fn date_sample(date: NaiveDate) -> SubscriptionDraft {
        SubscriptionDraft {
            user: uref("12345"),
            schedule: Schedule::Date(date),
            time_range: range(18 * 60, 22 * 60),
            courts: None,
        }
    }

    #[tokio::test]
    async fn date_schedule_roundtrips() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 6, 23).unwrap();
        store.add(date_sample(date)).await.unwrap();
        let subs = store
            .list_for_user(&uref("12345"), query_date())
            .await
            .unwrap();
        assert_eq!(subs[0].schedule, Schedule::Date(date));
    }

    #[tokio::test]
    async fn remove_any_deletes_regardless_of_owner() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let id = store.add(sample(Weekday::Tue)).await.unwrap();
        assert!(!store.remove(id, &uref("99999")).await.unwrap());
        assert!(store.remove_any(id).await.unwrap());
        assert!(!store.remove_any(id).await.unwrap()); // already gone
    }

    #[tokio::test]
    async fn remove_expired_deletes_only_past_dates() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 6, 15).unwrap();
        let past = NaiveDate::from_ymd_opt(2026, 6, 14).unwrap();
        let future = NaiveDate::from_ymd_opt(2026, 6, 16).unwrap();

        store.add(date_sample(past)).await.unwrap();
        store.add(date_sample(today)).await.unwrap();
        store.add(date_sample(future)).await.unwrap();
        store.add(sample(Weekday::Tue)).await.unwrap(); // recurring, never expires

        let removed = store.remove_expired(today).await.unwrap();
        assert_eq!(removed, 1); // only `past`

        let remaining = store.list_all(query_date()).await.unwrap();
        assert_eq!(remaining.len(), 3);
        assert!(!remaining.iter().any(|s| s.schedule == Schedule::Date(past)));
        assert!(
            remaining
                .iter()
                .any(|s| s.schedule == Schedule::Date(today))
        );
    }

    #[tokio::test]
    async fn active_lists_filter_expired_dates_without_deleting_them() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 6, 15).unwrap();
        let past = NaiveDate::from_ymd_opt(2026, 6, 14).unwrap();
        store.add(date_sample(past)).await.unwrap();
        store.add(date_sample(today)).await.unwrap();
        store.add(sample(Weekday::Tue)).await.unwrap();

        assert_eq!(
            store
                .list_for_user(&uref("12345"), today)
                .await
                .unwrap()
                .len(),
            2
        );
        assert_eq!(store.list_all(today).await.unwrap().len(), 2);
        assert_eq!(store.list_all(query_date()).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn both_schedule_types_insert_cleanly() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        store.add(sample(Weekday::Tue)).await.unwrap();
        store
            .add(date_sample(NaiveDate::from_ymd_opt(2026, 6, 23).unwrap()))
            .await
            .unwrap();
        assert_eq!(store.list_all(query_date()).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn weekday_roundtrips_for_all_days() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        for w in [
            Weekday::Mon,
            Weekday::Tue,
            Weekday::Wed,
            Weekday::Thu,
            Weekday::Fri,
            Weekday::Sat,
            Weekday::Sun,
        ] {
            store.add(sample(w)).await.unwrap();
        }
        let all = store.list_all(query_date()).await.unwrap();
        let schedules: Vec<Schedule> = all.iter().map(|s| s.schedule).collect();
        assert_eq!(
            schedules,
            vec![
                Schedule::Weekday(Weekday::Mon),
                Schedule::Weekday(Weekday::Tue),
                Schedule::Weekday(Weekday::Wed),
                Schedule::Weekday(Weekday::Thu),
                Schedule::Weekday(Weekday::Fri),
                Schedule::Weekday(Weekday::Sat),
                Schedule::Weekday(Weekday::Sun),
            ]
        );
    }
}
