use uuid::Uuid;

use crate::model::{CourtFilter, Schedule, Sport, VenueId};

use super::{
    AvailableSlotSummary, ClubRef, CourtSummary, OwnedSubscriptionSummary, SubscriptionSummary,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CourtChoice {
    Names(Vec<String>),
    Ids(Vec<Uuid>),
}

#[derive(Debug, Clone)]
pub enum SubscriptionCommand {
    Subscribe {
        sport: Sport,
        venue: Option<VenueId>,
        schedule: Schedule,
        start_minute: u32,
        end_minute: u32,
        courts: Option<CourtChoice>,
        filter: Option<CourtFilter>,
    },
    List,
    Unsubscribe {
        id: i64,
    },
    ListAll,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriptionResult {
    Subscribed {
        summary: SubscriptionSummary,
        open_slots: Vec<AvailableSlotSummary>,
    },
    SubscriptionList(Vec<SubscriptionSummary>),
    Unsubscribed {
        id: i64,
    },
    NotFound {
        id: i64,
    },
    InvalidTimeRange,
    InvalidSchedule,
    TooManyCourts {
        max: usize,
    },
    CourtNameTooLong {
        max: usize,
    },
    UnknownCourts {
        unknown: CourtChoice,
        available: Vec<CourtSummary>,
    },
    CourtsNeedClub,
    CourtCatalogsNotReady,
    UnknownClub {
        unknown: String,
        available: Vec<ClubRef>,
    },
    NoClubsConfigured {
        sport: Sport,
    },
    FilterExcludesCourts {
        courts: Vec<String>,
        filter: CourtFilter,
    },
    FilterNotForSport {
        sport: Sport,
        filter: CourtFilter,
    },
    AllSubscriptions(Vec<OwnedSubscriptionSummary>),
    NotAuthorized,
}
