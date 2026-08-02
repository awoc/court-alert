use anyhow::{Context, Result};
use chrono::{NaiveDate, Weekday};
use rusqlite::Row;

use crate::model::{CourtFilter, ProviderUserRef, Schedule, Subscription, TimeRange};

use super::DbRepr;

pub(super) struct SubscriptionRow {
    id: i64,
    provider: String,
    user_id: String,
    weekday: Option<u8>,
    on_date: Option<String>,
    start_minute: u32,
    end_minute: u32,
    courts: Option<String>,
    surface: String,
}

impl TryFrom<&Row<'_>> for SubscriptionRow {
    type Error = rusqlite::Error;

    fn try_from(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            provider: row.get("provider")?,
            user_id: row.get("user_id")?,
            weekday: row.get("weekday")?,
            on_date: row.get("on_date")?,
            start_minute: row.get("start_minute")?,
            end_minute: row.get("end_minute")?,
            courts: row.get("courts")?,
            surface: row.get("surface")?,
        })
    }
}

impl TryFrom<SubscriptionRow> for Subscription {
    type Error = rusqlite::Error;

    fn try_from(row: SubscriptionRow) -> rusqlite::Result<Self> {
        let time_range = TimeRange::new(row.start_minute, row.end_minute).ok_or_else(|| {
            conversion_error(format!(
                "invalid stored time range {}..{}",
                row.start_minute, row.end_minute
            ))
        })?;
        Ok(Self {
            id: row.id,
            user: ProviderUserRef {
                provider: row.provider,
                user_id: row.user_id,
            },
            schedule: Schedule::from_db((row.weekday, row.on_date))?,
            time_range,
            courts: Option::<Vec<String>>::from_db(row.courts)?,
            filter: CourtFilter::from_db(row.surface)?,
        })
    }
}

fn conversion_error(message: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, message.into())
}

impl DbRepr for Schedule {
    type Db = (Option<u8>, Option<String>);

    fn into_db(self) -> Result<Self::Db> {
        Ok(match self {
            Schedule::Weekday(weekday) => (Some(weekday.num_days_from_monday() as u8), None),
            Schedule::Date(date) => (None, Some(date.format("%Y-%m-%d").to_string())),
        })
    }

    fn from_db((weekday, on_date): Self::Db) -> rusqlite::Result<Self> {
        match (weekday, on_date) {
            (Some(number), None) => {
                Weekday::try_from(number)
                    .map(Schedule::Weekday)
                    .map_err(|error| {
                        conversion_error(format!("invalid stored weekday {number}: {error}"))
                    })
            }
            (None, Some(date)) => NaiveDate::parse_from_str(&date, "%Y-%m-%d")
                .map(Schedule::Date)
                .map_err(|error| {
                    conversion_error(format!("invalid stored on_date {date:?}: {error}"))
                }),
            _ => Err(conversion_error(
                "subscription row has neither or both of weekday/on_date".into(),
            )),
        }
    }
}

impl DbRepr for CourtFilter {
    type Db = String;

    /// The column still spells `Any` as `'all'` and admits only the tennis
    /// vocabulary; widening it is migration 0005's job, alongside the rename to
    /// `court_filter`.
    fn into_db(self) -> Result<Self::Db> {
        Ok(match self {
            Self::Any => "all".to_string(),
            filter => filter.to_string(),
        })
    }

    fn from_db(db: Self::Db) -> rusqlite::Result<Self> {
        db.parse().map_err(|error| {
            conversion_error(format!("invalid stored court filter {db:?}: {error}"))
        })
    }
}

impl DbRepr for Option<Vec<String>> {
    type Db = Option<String>;

    fn into_db(self) -> Result<Self::Db> {
        self.map(|courts| serde_json::to_string(&courts).context("serializing courts list"))
            .transpose()
    }

    fn from_db(db: Self::Db) -> rusqlite::Result<Self> {
        match db {
            None => Ok(None),
            Some(text) => serde_json::from_str(&text)
                .map_err(|error| conversion_error(format!("invalid stored courts JSON: {error}"))),
        }
    }
}
