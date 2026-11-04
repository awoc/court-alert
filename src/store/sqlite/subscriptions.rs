use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::NaiveDate;
use rusqlite::{Row, params};

use crate::model::{
    CourtSelection, SelectedCourt, SubscriberRef, Subscription, SubscriptionDraft, VenueId,
};
use crate::ports::SubscriptionRepository;

use super::{DbRepr, SqliteStore, SubscriptionRow};

const SUB_COLUMNS: &str = "id, provider, user_id, sport, venue, weekday, on_date, \
                           start_minute, end_minute, courts, selected_courts, court_filter";

fn map_subscription(row: &Row<'_>) -> rusqlite::Result<Subscription> {
    Subscription::try_from(SubscriptionRow::try_from(row)?)
}

#[async_trait]
impl SubscriptionRepository for SqliteStore {
    async fn add(&self, sub: SubscriptionDraft) -> Result<i64> {
        self.with_writer("insert", move |conn| {
            let (weekday, on_date) = sub.schedule().into_db()?;
            let (courts, selected_courts) = sub.courts().clone().into_db()?;
            let filter = sub.filter().into_db()?;
            let sport = sub.sport().into_db()?;
            conn.execute(
                "INSERT INTO subscriptions
                 (provider, user_id, sport, venue, weekday, on_date,
                  start_minute, end_minute, courts, selected_courts, court_filter)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    sub.subscriber().integration.as_str(),
                    sub.subscriber().subscriber_id,
                    sport,
                    sub.venue().map(VenueId::to_string),
                    weekday,
                    on_date,
                    sub.time_range().start_minute(),
                    sub.time_range().end_minute(),
                    courts,
                    selected_courts,
                    filter,
                ],
            )
            .context("inserting subscription")?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    async fn list_for_subscriber(
        &self,
        subscriber: &SubscriberRef,
        today: NaiveDate,
    ) -> Result<Vec<Subscription>> {
        let subscriber = subscriber.clone();
        let today = today.format("%Y-%m-%d").to_string();
        self.with_reader("list_for_subscriber", move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {SUB_COLUMNS}
                     FROM subscriptions
                     WHERE provider = ?1 AND user_id = ?2
                       AND (on_date IS NULL OR on_date >= ?3)
                     ORDER BY id"
                ))
                .context("preparing list_for_subscriber")?;
            let rows = stmt
                .query_map(
                    params![
                        subscriber.integration.as_str(),
                        subscriber.subscriber_id,
                        today
                    ],
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
        self.with_reader("list_all", move |conn| {
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

    async fn remove(&self, id: i64, subscriber: &SubscriberRef) -> Result<bool> {
        let subscriber = subscriber.clone();
        self.with_writer("remove", move |conn| {
            let affected = conn
                .execute(
                    "DELETE FROM subscriptions
                     WHERE id = ?1 AND provider = ?2 AND user_id = ?3",
                    params![
                        id,
                        subscriber.integration.as_str(),
                        subscriber.subscriber_id
                    ],
                )
                .context("deleting subscription")?;
            Ok(affected > 0)
        })
        .await
    }

    async fn remove_any(&self, id: i64) -> Result<bool> {
        self.with_writer("remove_any", move |conn| {
            let affected = conn
                .execute("DELETE FROM subscriptions WHERE id = ?1", params![id])
                .context("deleting subscription by id")?;
            Ok(affected > 0)
        })
        .await
    }

    async fn remove_expired(&self, today: chrono::NaiveDate) -> Result<u64> {
        let today = today.format("%Y-%m-%d").to_string();
        self.with_writer("remove_expired", move |conn| {
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

    async fn convert_pending_courts(&self, id: i64, courts: Vec<SelectedCourt>) -> Result<bool> {
        self.with_writer("convert_pending_courts", move |conn| {
            let (_, selected_courts) = CourtSelection::Selected(courts).into_db()?;
            let affected = conn
                .execute(
                    "UPDATE subscriptions SET courts = NULL, selected_courts = ?2
                     WHERE id = ?1 AND courts IS NOT NULL",
                    params![id, selected_courts],
                )
                .context("converting pending courts")?;
            Ok(affected > 0)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        CourtFilter, CourtLocation, CourtSurface, Schedule, Sport, TimeRange, UncheckedDraft,
    };
    use chrono::{NaiveDate, Weekday};
    use uuid::Uuid;

    fn uref(id: &str) -> SubscriberRef {
        SubscriberRef {
            integration: "discord".into(),
            subscriber_id: id.into(),
        }
    }

    fn query_date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2000, 1, 1).unwrap()
    }

    fn range(from: u32, to: u32) -> TimeRange {
        TimeRange::new(from, to).unwrap()
    }

    fn sample(weekday: Weekday) -> SubscriptionDraft {
        unchecked(weekday).assume_valid()
    }

    fn unchecked(weekday: Weekday) -> UncheckedDraft {
        UncheckedDraft {
            subscriber: uref("12345"),
            sport: Sport::Tennis,
            venue: None,
            schedule: Schedule::Weekday(weekday),
            time_range: range(18 * 60, 22 * 60),
            courts: CourtSelection::Every,
            filter: CourtFilter::CLAY,
        }
    }

    #[tokio::test]
    async fn add_then_list_for_subscriber_returns_inserted_row() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let id = store.add(sample(Weekday::Tue)).await.unwrap();
        let subs = store
            .list_for_subscriber(&uref("12345"), query_date())
            .await
            .unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].id, id);
        assert_eq!(subs[0].subscriber, uref("12345"));
        assert_eq!(subs[0].schedule, Schedule::Weekday(Weekday::Tue));
        assert_eq!(subs[0].time_range, range(1080, 1320));
        assert_eq!(subs[0].courts, CourtSelection::Every);
        assert_eq!(subs[0].filter, CourtFilter::CLAY);
    }

    #[tokio::test]
    async fn court_filter_roundtrips() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let filters = [
            CourtFilter::Any,
            CourtFilter::CLAY,
            CourtFilter::Surface(CourtSurface::Synthetic),
            CourtFilter::Location(CourtLocation::Indoor),
            CourtFilter::Location(CourtLocation::Outdoor),
        ];
        for filter in filters {
            let mut sub = unchecked(Weekday::Tue);
            sub.filter = filter;
            store.add(sub.assume_valid()).await.unwrap();
        }
        let stored: Vec<CourtFilter> = store
            .list_all(query_date())
            .await
            .unwrap()
            .iter()
            .map(|sub| sub.filter)
            .collect();
        assert_eq!(stored, filters);
    }

    #[tokio::test]
    async fn list_for_subscriber_filters_by_subscriber_and_integration() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        store.add(sample(Weekday::Tue)).await.unwrap();
        let mut other_subscriber = unchecked(Weekday::Wed);
        other_subscriber.subscriber = uref("99999");
        store.add(other_subscriber.assume_valid()).await.unwrap();
        let mut other_integration = unchecked(Weekday::Thu);
        other_integration.subscriber = SubscriberRef {
            integration: "telegram".into(),
            subscriber_id: "12345".into(),
        };
        store.add(other_integration.assume_valid()).await.unwrap();

        let subs = store
            .list_for_subscriber(&uref("12345"), query_date())
            .await
            .unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].schedule, Schedule::Weekday(Weekday::Tue));
    }

    #[tokio::test]
    async fn list_all_returns_everything() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        store.add(sample(Weekday::Tue)).await.unwrap();
        let mut other = unchecked(Weekday::Wed);
        other.subscriber = uref("99999");
        store.add(other.assume_valid()).await.unwrap();
        assert_eq!(store.list_all(query_date()).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn remove_only_succeeds_for_owner() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let id = store.add(sample(Weekday::Tue)).await.unwrap();
        assert!(!store.remove(id, &uref("99999")).await.unwrap());
        let foreign = SubscriberRef {
            integration: "telegram".into(),
            subscriber_id: "12345".into(),
        };
        assert!(!store.remove(id, &foreign).await.unwrap());
        assert!(store.remove(id, &uref("12345")).await.unwrap());
        assert_eq!(
            store
                .list_for_subscriber(&uref("12345"), query_date())
                .await
                .unwrap()
                .len(),
            0
        );
    }

    async fn round_trip(courts: CourtSelection) -> CourtSelection {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let mut sub = unchecked(Weekday::Tue);
        sub.courts = courts;
        store.add(sub.assume_valid()).await.unwrap();
        store
            .list_for_subscriber(&uref("12345"), query_date())
            .await
            .unwrap()
            .remove(0)
            .courts
    }

    #[tokio::test]
    async fn selected_courts_roundtrip_with_their_venues() {
        let selection = CourtSelection::Selected(vec![
            SelectedCourt {
                venue_id: VenueId::new("zhs-munich"),
                court_id: Uuid::from_u128(2),
            },
            SelectedCourt {
                venue_id: VenueId::new("casa-padel"),
                court_id: Uuid::from_u128(101),
            },
        ]);
        assert_eq!(round_trip(selection.clone()).await, selection);
    }

    #[tokio::test]
    async fn pending_court_names_roundtrip() {
        let selection = CourtSelection::PendingNames(vec!["Court 2".into(), "Court 5".into()]);
        assert_eq!(round_trip(selection.clone()).await, selection);
    }

    fn court(venue: &str, id: u128) -> SelectedCourt {
        SelectedCourt {
            venue_id: VenueId::new(venue),
            court_id: Uuid::from_u128(id),
        }
    }

    async fn stored_courts(store: &SqliteStore) -> CourtSelection {
        store.list_all(query_date()).await.unwrap().remove(0).courts
    }

    #[tokio::test]
    async fn converting_replaces_pending_names_with_courts() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let mut sub = unchecked(Weekday::Tue);
        sub.courts = CourtSelection::PendingNames(vec!["Court 2".into()]);
        let id = store.add(sub.assume_valid()).await.unwrap();

        let courts = vec![court("zhs-munich", 2)];
        assert!(
            store
                .convert_pending_courts(id, courts.clone())
                .await
                .unwrap()
        );

        assert_eq!(
            stored_courts(&store).await,
            CourtSelection::Selected(courts)
        );
    }

    #[tokio::test]
    async fn converting_twice_keeps_the_first_result() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let mut sub = unchecked(Weekday::Tue);
        sub.courts = CourtSelection::PendingNames(vec!["Court 2".into()]);
        let id = store.add(sub.assume_valid()).await.unwrap();
        let first = vec![court("zhs-munich", 2)];
        store
            .convert_pending_courts(id, first.clone())
            .await
            .unwrap();

        let again = store
            .convert_pending_courts(id, vec![court("zhs-munich", 5)])
            .await
            .unwrap();

        assert!(!again);
        assert_eq!(stored_courts(&store).await, CourtSelection::Selected(first));
    }

    #[tokio::test]
    async fn converting_leaves_rows_that_are_not_pending_alone() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let id = store.add(sample(Weekday::Tue)).await.unwrap();

        let converted = store
            .convert_pending_courts(id, vec![court("zhs-munich", 2)])
            .await
            .unwrap();

        assert!(!converted);
        assert_eq!(stored_courts(&store).await, CourtSelection::Every);
        assert!(
            !store
                .convert_pending_courts(id + 1, vec![court("zhs-munich", 2)])
                .await
                .unwrap(),
            "a missing row converts nothing"
        );
    }

    #[tokio::test]
    async fn a_row_with_neither_kind_of_courts_watches_every_court() {
        assert_eq!(
            round_trip(CourtSelection::Every).await,
            CourtSelection::Every
        );
    }

    fn date_sample(date: NaiveDate) -> SubscriptionDraft {
        UncheckedDraft {
            subscriber: uref("12345"),
            sport: Sport::Tennis,
            venue: None,
            schedule: Schedule::Date(date),
            time_range: range(18 * 60, 22 * 60),
            courts: CourtSelection::Every,
            filter: CourtFilter::CLAY,
        }
        .assume_valid()
    }

    #[tokio::test]
    async fn date_schedule_roundtrips() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 6, 23).unwrap();
        store.add(date_sample(date)).await.unwrap();
        let subs = store
            .list_for_subscriber(&uref("12345"), query_date())
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
                .list_for_subscriber(&uref("12345"), today)
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
