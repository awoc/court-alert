use chrono::{NaiveDate, Weekday};

use super::{CourtFilter, Sport, VenueId};

pub const MINUTES_PER_DAY: u32 = 24 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProviderUserRef {
    pub provider: String,
    pub user_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Schedule {
    Weekday(Weekday),
    Date(NaiveDate),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimeRange {
    start_minute: u32,
    end_minute: u32,
}

impl TimeRange {
    pub fn new(start_minute: u32, end_minute: u32) -> Option<Self> {
        (start_minute < end_minute && end_minute <= MINUTES_PER_DAY).then_some(Self {
            start_minute,
            end_minute,
        })
    }

    pub fn start_minute(self) -> u32 {
        self.start_minute
    }

    pub fn end_minute(self) -> u32 {
        self.end_minute
    }

    pub fn contains(self, minute_of_day: u32) -> bool {
        (self.start_minute..self.end_minute).contains(&minute_of_day)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    pub id: i64,
    pub user: ProviderUserRef,
    /// Which command created this, and therefore which venues it covers.
    ///
    /// Not redundant with `venue`: `/subscribe` and `/padel` can both produce
    /// `filter = Any, venue = None`, and `'any'` cannot say which sport it
    /// meant. Scoping by sport rather than by provider is also what lets a
    /// future Playtomic *tennis* club fall under `/subscribe` automatically.
    pub sport: Sport,
    /// `None` = every venue of that sport.
    pub venue: Option<VenueId>,
    pub schedule: Schedule,
    pub time_range: TimeRange,
    pub courts: Option<Vec<String>>,
    pub filter: CourtFilter,
}

#[derive(Debug, Clone)]
pub struct SubscriptionDraft {
    pub user: ProviderUserRef,
    pub sport: Sport,
    pub venue: Option<VenueId>,
    pub schedule: Schedule,
    pub time_range: TimeRange,
    pub courts: Option<Vec<String>>,
    pub filter: CourtFilter,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_range_accepts_valid_half_open_ranges() {
        assert!(TimeRange::new(0, MINUTES_PER_DAY).is_some());
        assert!(TimeRange::new(18 * 60, 20 * 60).is_some());
    }

    #[test]
    fn time_range_rejects_empty_inverted_or_multi_day() {
        assert!(TimeRange::new(18 * 60, 18 * 60).is_none());
        assert!(TimeRange::new(20 * 60, 18 * 60).is_none());
        assert!(TimeRange::new(23 * 60, MINUTES_PER_DAY + 60).is_none());
    }

    #[test]
    fn time_range_containment_is_half_open() {
        let range = TimeRange::new(18 * 60, 20 * 60).unwrap();
        assert!(range.contains(18 * 60));
        assert!(range.contains(19 * 60));
        assert!(!range.contains(20 * 60));
    }
}
