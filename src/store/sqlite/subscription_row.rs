use anyhow::{Context, Result};
use chrono::{NaiveDate, Weekday};
use rusqlite::Row;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::{
    CourtFilter, CourtSelection, Schedule, SelectedCourt, Sport, SubscriberRef, Subscription,
    TimeRange, VenueId,
};

use super::DbRepr;

pub(super) struct SubscriptionRow {
    id: i64,
    provider: String,
    user_id: String,
    sport: String,
    venue: Option<String>,
    weekday: Option<u8>,
    on_date: Option<String>,
    start_minute: u32,
    end_minute: u32,
    courts: Option<String>,
    selected_courts: Option<String>,
    court_filter: String,
}

impl TryFrom<&Row<'_>> for SubscriptionRow {
    type Error = rusqlite::Error;

    fn try_from(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            provider: row.get("provider")?,
            user_id: row.get("user_id")?,
            sport: row.get("sport")?,
            venue: row.get("venue")?,
            weekday: row.get("weekday")?,
            on_date: row.get("on_date")?,
            start_minute: row.get("start_minute")?,
            end_minute: row.get("end_minute")?,
            courts: row.get("courts")?,
            selected_courts: row.get("selected_courts")?,
            court_filter: row.get("court_filter")?,
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
            subscriber: SubscriberRef {
                integration: row.provider.into(),
                subscriber_id: row.user_id,
            },
            sport: Sport::from_db(row.sport)?,
            venue: row.venue.map(VenueId::from),
            schedule: Schedule::from_db((row.weekday, row.on_date))?,
            time_range,
            courts: CourtSelection::from_db((row.courts, row.selected_courts))?,
            filter: CourtFilter::from_db(row.court_filter)?,
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

    fn into_db(self) -> Result<Self::Db> {
        Ok(self.to_string())
    }

    fn from_db(db: Self::Db) -> rusqlite::Result<Self> {
        db.parse().map_err(|error| {
            conversion_error(format!("invalid stored court filter {db:?}: {error}"))
        })
    }
}

impl DbRepr for Sport {
    type Db = String;

    fn into_db(self) -> Result<Self::Db> {
        Ok(self.to_string())
    }

    fn from_db(db: Self::Db) -> rusqlite::Result<Self> {
        db.parse()
            .map_err(|error| conversion_error(format!("invalid stored sport {db:?}: {error}")))
    }
}

#[derive(Serialize, Deserialize)]
struct StoredCourt {
    venue_id: String,
    court_id: Uuid,
}

impl From<SelectedCourt> for StoredCourt {
    fn from(court: SelectedCourt) -> Self {
        Self {
            venue_id: court.venue_id.to_string(),
            court_id: court.court_id,
        }
    }
}

impl From<StoredCourt> for SelectedCourt {
    fn from(court: StoredCourt) -> Self {
        Self {
            venue_id: court.venue_id.into(),
            court_id: court.court_id,
        }
    }
}

impl DbRepr for CourtSelection {
    /// `(courts, selected_courts)`: the names of a pending selection, or the
    /// identities of a converted one.
    type Db = (Option<String>, Option<String>);

    fn into_db(self) -> Result<Self::Db> {
        Ok(match self {
            Self::Every => (None, None),
            Self::PendingNames(names) => (
                Some(serde_json::to_string(&names).context("serializing court names")?),
                None,
            ),
            Self::Selected(courts) => {
                let stored: Vec<StoredCourt> = courts.into_iter().map(Into::into).collect();
                (
                    None,
                    Some(serde_json::to_string(&stored).context("serializing selected courts")?),
                )
            }
        })
    }

    fn from_db(db: Self::Db) -> rusqlite::Result<Self> {
        match db {
            (None, None) => Ok(Self::Every),
            (Some(names), None) => serde_json::from_str(&names)
                .map(Self::PendingNames)
                .map_err(|error| conversion_error(format!("invalid stored courts JSON: {error}"))),
            (None, Some(courts)) => serde_json::from_str::<Vec<StoredCourt>>(&courts)
                .map(|stored| Self::Selected(stored.into_iter().map(Into::into).collect()))
                .map_err(|error| {
                    conversion_error(format!("invalid stored selected_courts JSON: {error}"))
                }),
            (Some(_), Some(_)) => Err(conversion_error(
                "subscription row has both courts and selected_courts".into(),
            )),
        }
    }
}
