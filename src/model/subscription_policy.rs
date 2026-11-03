use std::collections::HashSet;
use std::sync::Arc;

use chrono::NaiveDate;
use uuid::Uuid;

use super::{
    Court, CourtCatalog, CourtFilter, CourtSelection, Schedule, SelectedCourt, Sport,
    SubscriberRef, SubscriptionDraft, TimeRange, VenueId, VenueRegistry,
};

const MAX_COURTS: usize = 20;
const MAX_COURT_NAME_CHARS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CourtChoice {
    Names(Vec<String>),
    Ids(Vec<Uuid>),
}

impl CourtChoice {
    fn count(&self) -> usize {
        match self {
            Self::Names(names) => names.len(),
            Self::Ids(ids) => ids.len(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SubscriptionRequest {
    pub sport: Sport,
    pub venue: Option<VenueId>,
    pub schedule: Schedule,
    pub start_minute: u32,
    pub end_minute: u32,
    pub courts: Option<CourtChoice>,
    pub filter: Option<CourtFilter>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriptionRejection {
    NoClubsConfigured {
        sport: Sport,
    },
    UnknownClub {
        unknown: VenueId,
    },
    InvalidTimeRange,
    InvalidSchedule,
    FilterNotForSport {
        sport: Sport,
        filter: CourtFilter,
    },
    TooManyCourts {
        max: usize,
    },
    CourtNameTooLong {
        max: usize,
    },
    CourtCatalogsNotReady,
    CourtsNeedClub,
    UnknownCourts {
        unknown: CourtChoice,
    },
    FilterExcludesCourts {
        courts: Vec<String>,
        filter: CourtFilter,
    },
}

/// What a new subscription is checked against. Rows already stored are never
/// put through it, because the catalogs, the date and these rules move on after
/// a row is written.
pub struct SubscriptionPolicy<'a> {
    pub registry: &'a VenueRegistry,
    pub tennis_default_filter: CourtFilter,
    pub today: NaiveDate,
}

impl SubscriptionPolicy<'_> {
    pub fn draft(
        &self,
        subscriber: &SubscriberRef,
        request: &SubscriptionRequest,
    ) -> Result<SubscriptionDraft, SubscriptionRejection> {
        let sport = request.sport;
        let clubs = self.registry.venues_of_sport(sport);
        if clubs.is_empty() {
            return Err(SubscriptionRejection::NoClubsConfigured { sport });
        }
        if let Some(chosen) = &request.venue
            && !clubs.contains(chosen)
        {
            return Err(SubscriptionRejection::UnknownClub {
                unknown: chosen.clone(),
            });
        }

        let time_range = TimeRange::new(request.start_minute, request.end_minute)
            .ok_or(SubscriptionRejection::InvalidTimeRange)?;
        if let Schedule::Date(date) = request.schedule
            && date < self.today
        {
            return Err(SubscriptionRejection::InvalidSchedule);
        }

        if let Some(filter) = request.filter
            && !filter.applies_to(sport)
        {
            return Err(SubscriptionRejection::FilterNotForSport { sport, filter });
        }

        let courts = self.select_courts(request)?;

        if let Some(filter) = request.filter
            && let Some(excluded) = self.courts_excluded_by(filter, &courts)
        {
            return Err(SubscriptionRejection::FilterExcludesCourts {
                courts: excluded,
                filter,
            });
        }

        Ok(SubscriptionDraft {
            subscriber: subscriber.clone(),
            sport,
            venue: request.venue.clone(),
            schedule: request.schedule,
            time_range,
            filter: self.resolve_filter(sport, request.filter, &courts),
            courts,
        })
    }

    fn select_courts(
        &self,
        request: &SubscriptionRequest,
    ) -> Result<CourtSelection, SubscriptionRejection> {
        let Some(courts) = &request.courts else {
            return Ok(CourtSelection::Every);
        };
        if courts.count() == 0 {
            return Ok(CourtSelection::Every);
        }
        check_input_limits(courts)?;
        let resolved = match courts {
            CourtChoice::Names(names) => {
                let catalogs = self
                    .registry
                    .ready_catalogs(request.sport, request.venue.as_ref())
                    .ok_or(SubscriptionRejection::CourtCatalogsNotReady)?;
                courts_by_name(&catalogs, names).map_err(CourtChoice::Names)
            }
            CourtChoice::Ids(ids) => {
                let venue = request
                    .venue
                    .as_ref()
                    .ok_or(SubscriptionRejection::CourtsNeedClub)?;
                self.courts_by_id(venue, ids).map_err(CourtChoice::Ids)
            }
        };
        resolved
            .map(CourtSelection::Selected)
            .map_err(|unknown| SubscriptionRejection::UnknownCourts { unknown })
    }

    fn courts_by_id(&self, venue: &VenueId, ids: &[Uuid]) -> Result<Vec<SelectedCourt>, Vec<Uuid>> {
        let catalog = self.registry.catalog(venue);
        let mut selected = Vec::with_capacity(ids.len());
        let mut unknown = Vec::new();
        for &id in ids {
            match catalog.as_deref().and_then(|known| known.find_by_id(id)) {
                Some(court) => selected.push(SelectedCourt {
                    venue_id: venue.clone(),
                    court_id: court.id(),
                }),
                None => unknown.push(id),
            }
        }
        if unknown.is_empty() {
            Ok(distinct_courts(selected))
        } else {
            Err(unknown)
        }
    }

    fn courts_excluded_by(
        &self,
        filter: CourtFilter,
        courts: &CourtSelection,
    ) -> Option<Vec<String>> {
        let CourtSelection::Selected(courts) = courts else {
            return None;
        };
        let excluded: Vec<String> = courts
            .iter()
            .filter(|court| {
                let attributes = self.registry.attributes_of(&court.venue_id, court.court_id);
                !filter.allows(attributes.as_ref())
            })
            .map(|court| {
                self.registry
                    .court_name(&court.venue_id, court.court_id)
                    .unwrap_or_else(|| court.court_id.to_string())
            })
            .collect();
        (!excluded.is_empty()).then_some(excluded)
    }

    fn resolve_filter(
        &self,
        sport: Sport,
        chosen: Option<CourtFilter>,
        courts: &CourtSelection,
    ) -> CourtFilter {
        chosen.unwrap_or(match (sport, courts) {
            (Sport::Tennis, CourtSelection::Every) => self.tennis_default_filter,
            _ => CourtFilter::Any,
        })
    }
}

/// These bound what was asked for, not what a subscription may hold: one name
/// can select more than `MAX_COURTS` courts, and a selection keeps no names.
fn check_input_limits(courts: &CourtChoice) -> Result<(), SubscriptionRejection> {
    if courts.count() > MAX_COURTS {
        return Err(SubscriptionRejection::TooManyCourts { max: MAX_COURTS });
    }
    if let CourtChoice::Names(names) = courts
        && names
            .iter()
            .any(|name| name.chars().count() > MAX_COURT_NAME_CHARS)
    {
        return Err(SubscriptionRejection::CourtNameTooLong {
            max: MAX_COURT_NAME_CHARS,
        });
    }
    Ok(())
}

/// A name found anywhere in scope selects every court of that name. Only an
/// input no court is called falls back to the number it contains, so "Court 2"
/// never picks up another club's "Pista 2" but "2" picks up both.
fn courts_by_name(
    catalogs: &[(VenueId, Arc<CourtCatalog>)],
    names: &[String],
) -> Result<Vec<SelectedCourt>, Vec<String>> {
    resolve_names(names, |name| {
        let input = name.trim();
        let found = select_from(catalogs, |catalog| catalog.courts_named(input));
        if found.is_empty() {
            select_from(catalogs, |catalog| catalog.courts_numbered_in(input))
        } else {
            found
        }
    })
}

/// Stored names are already canonical, so number shorthand does not apply.
pub fn courts_with_stored_names(
    catalogs: &[(VenueId, Arc<CourtCatalog>)],
    names: &[String],
) -> Result<Vec<SelectedCourt>, Vec<String>> {
    resolve_names(names, |name| {
        select_from(catalogs, |catalog| catalog.courts_named(name))
    })
}

fn resolve_names(
    names: &[String],
    courts_for: impl Fn(&str) -> Vec<SelectedCourt>,
) -> Result<Vec<SelectedCourt>, Vec<String>> {
    let mut selected = Vec::with_capacity(names.len());
    let mut unresolved = Vec::new();
    for name in names {
        let mut found = courts_for(name);
        if found.is_empty() {
            unresolved.push(name.clone());
        } else {
            selected.append(&mut found);
        }
    }
    if unresolved.is_empty() {
        Ok(distinct_courts(selected))
    } else {
        Err(unresolved)
    }
}

fn select_from<'a, I>(
    catalogs: &'a [(VenueId, Arc<CourtCatalog>)],
    courts_of: impl Fn(&'a CourtCatalog) -> I,
) -> Vec<SelectedCourt>
where
    I: Iterator<Item = &'a Court>,
{
    catalogs
        .iter()
        .flat_map(|(venue_id, catalog)| {
            courts_of(catalog).map(|court| SelectedCourt {
                venue_id: venue_id.clone(),
                court_id: court.id(),
            })
        })
        .collect()
}

fn distinct_courts(mut courts: Vec<SelectedCourt>) -> Vec<SelectedCourt> {
    let mut seen = HashSet::new();
    courts.retain(|court| seen.insert(court.clone()));
    courts
}

#[cfg(test)]
mod tests {
    use chrono::Weekday;

    use super::*;
    use crate::model::{
        CourtAttributes, CourtLocation, CourtSurface, Subscription, Venue, VenueIdentity,
    };

    use SubscriptionRejection as Rejected;

    fn tennis(id: &str) -> Venue {
        Venue {
            id: VenueId::new(id),
            display_name: id.into(),
            sport: Sport::Tennis,
            identity: VenueIdentity::Zhs {
                base_url: "https://example.test".into(),
            },
            poll_interval_secs: None,
            lookahead_days: None,
            operating_window: None,
        }
    }

    fn padel(id: &str) -> Venue {
        Venue {
            sport: Sport::Padel,
            identity: VenueIdentity::Playtomic {
                tenant_id: Uuid::nil(),
                slug: id.into(),
            },
            ..tennis(id)
        }
    }

    fn clay(id: u128, name: &str) -> Court {
        Court::new(
            Uuid::from_u128(id),
            name.into(),
            CourtAttributes::tennis(CourtSurface::Clay),
        )
    }

    fn synthetic(id: u128, name: &str) -> Court {
        Court::new(
            Uuid::from_u128(id),
            name.into(),
            CourtAttributes::tennis(CourtSurface::Synthetic),
        )
    }

    fn registry_of(clubs: Vec<(Venue, Option<Vec<Court>>)>) -> VenueRegistry {
        let mut registry = VenueRegistry::new();
        for (club, courts) in clubs {
            registry.register(&club);
            if let Some(courts) = courts {
                registry.set_catalog(&club.id, CourtCatalog::new(courts));
            }
        }
        registry
    }

    /// `north` has clay courts 1 and 2 and synthetic court 3. `south` has clay
    /// court 4, sharing the name of north's court 1, and clay court 5, "Pista 2".
    fn two_clubs() -> VenueRegistry {
        registry_of(vec![
            (
                tennis("north"),
                Some(vec![
                    clay(1, "Court 1"),
                    clay(2, "Court 2"),
                    synthetic(3, "Court 3"),
                ]),
            ),
            (
                tennis("south"),
                Some(vec![clay(4, "Court 1"), clay(5, "Pista 2")]),
            ),
        ])
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 6, 15).unwrap()
    }

    fn policy(registry: &VenueRegistry) -> SubscriptionPolicy<'_> {
        SubscriptionPolicy {
            registry,
            tennis_default_filter: CourtFilter::CLAY,
            today: today(),
        }
    }

    fn request() -> SubscriptionRequest {
        SubscriptionRequest {
            sport: Sport::Tennis,
            venue: None,
            schedule: Schedule::Weekday(Weekday::Tue),
            start_minute: 18 * 60,
            end_minute: 20 * 60,
            courts: None,
            filter: None,
        }
    }

    fn north() -> Option<VenueId> {
        Some(VenueId::new("north"))
    }

    fn subscriber() -> SubscriberRef {
        SubscriberRef {
            integration: "discord".into(),
            subscriber_id: "1".into(),
        }
    }

    fn names(names: &[&str]) -> Option<CourtChoice> {
        Some(CourtChoice::Names(
            names.iter().map(|name| (*name).to_owned()).collect(),
        ))
    }

    fn ids(ids: &[u128]) -> Option<CourtChoice> {
        Some(CourtChoice::Ids(
            ids.iter().map(|id| Uuid::from_u128(*id)).collect(),
        ))
    }

    fn rejection(registry: &VenueRegistry, request: SubscriptionRequest) -> Rejected {
        policy(registry)
            .draft(&subscriber(), &request)
            .expect_err("the policy should reject this")
    }

    fn accepted(registry: &VenueRegistry, request: SubscriptionRequest) -> Subscription {
        policy(registry)
            .draft(&subscriber(), &request)
            .expect("the policy should accept this")
            .into_subscription(7)
    }

    fn selected_ids(sub: &Subscription) -> Vec<(&str, u128)> {
        let CourtSelection::Selected(courts) = &sub.courts else {
            panic!("expected selected courts, got {:?}", sub.courts);
        };
        courts
            .iter()
            .map(|court| (court.venue_id.as_str(), court.court_id.as_u128()))
            .collect()
    }

    #[test]
    fn an_accepted_request_keeps_what_was_asked_for() {
        let registry = two_clubs();
        let sub = accepted(
            &registry,
            SubscriptionRequest {
                venue: north(),
                schedule: Schedule::Date(today()),
                courts: ids(&[2]),
                filter: Some(CourtFilter::CLAY),
                ..request()
            },
        );

        assert_eq!(
            sub,
            Subscription {
                id: 7,
                subscriber: subscriber(),
                sport: Sport::Tennis,
                venue: north(),
                schedule: Schedule::Date(today()),
                time_range: TimeRange::new(18 * 60, 20 * 60).unwrap(),
                courts: CourtSelection::Selected(vec![SelectedCourt {
                    venue_id: VenueId::new("north"),
                    court_id: Uuid::from_u128(2),
                }]),
                filter: CourtFilter::CLAY,
            }
        );
    }

    #[test]
    fn a_sport_with_no_clubs_is_rejected_ahead_of_everything_else() {
        let registry = two_clubs();
        let reply = rejection(
            &registry,
            SubscriptionRequest {
                sport: Sport::Padel,
                venue: Some(VenueId::new("nowhere")),
                start_minute: 20 * 60,
                end_minute: 18 * 60,
                ..request()
            },
        );
        assert_eq!(
            reply,
            Rejected::NoClubsConfigured {
                sport: Sport::Padel
            }
        );
    }

    #[test]
    fn an_unknown_club_is_rejected_ahead_of_a_bad_time_range() {
        let registry = two_clubs();
        let reply = rejection(
            &registry,
            SubscriptionRequest {
                venue: Some(VenueId::new("nowhere")),
                start_minute: 20 * 60,
                end_minute: 18 * 60,
                ..request()
            },
        );
        assert_eq!(
            reply,
            Rejected::UnknownClub {
                unknown: VenueId::new("nowhere")
            }
        );
    }

    #[test]
    fn a_club_of_another_sport_is_an_unknown_club() {
        let registry = registry_of(vec![(tennis("north"), None), (padel("casa"), Some(vec![]))]);
        let reply = rejection(
            &registry,
            SubscriptionRequest {
                venue: Some(VenueId::new("casa")),
                ..request()
            },
        );
        assert_eq!(
            reply,
            Rejected::UnknownClub {
                unknown: VenueId::new("casa")
            }
        );
    }

    #[test]
    fn a_bad_time_range_is_rejected_ahead_of_a_past_date() {
        let registry = two_clubs();
        let reply = rejection(
            &registry,
            SubscriptionRequest {
                schedule: Schedule::Date(today().pred_opt().unwrap()),
                start_minute: 20 * 60,
                end_minute: 18 * 60,
                ..request()
            },
        );
        assert_eq!(reply, Rejected::InvalidTimeRange);
    }

    #[test]
    fn a_past_date_is_rejected_ahead_of_a_filter_for_another_sport() {
        let registry = two_clubs();
        let reply = rejection(
            &registry,
            SubscriptionRequest {
                schedule: Schedule::Date(today().pred_opt().unwrap()),
                filter: Some(CourtFilter::Location(CourtLocation::Indoor)),
                ..request()
            },
        );
        assert_eq!(reply, Rejected::InvalidSchedule);
    }

    #[test]
    fn today_is_not_a_past_date() {
        let registry = two_clubs();
        let sub = accepted(
            &registry,
            SubscriptionRequest {
                schedule: Schedule::Date(today()),
                ..request()
            },
        );
        assert_eq!(sub.schedule, Schedule::Date(today()));
    }

    #[test]
    fn a_filter_for_another_sport_is_rejected_ahead_of_any_court_limit() {
        let registry = two_clubs();
        let filter = CourtFilter::Location(CourtLocation::Indoor);
        let reply = rejection(
            &registry,
            SubscriptionRequest {
                courts: names(&["Court 1"; MAX_COURTS + 1]),
                filter: Some(filter),
                ..request()
            },
        );
        assert_eq!(
            reply,
            Rejected::FilterNotForSport {
                sport: Sport::Tennis,
                filter,
            }
        );
    }

    #[test]
    fn too_many_courts_is_rejected_ahead_of_a_long_name_or_a_missing_club() {
        let registry = two_clubs();
        let long = "x".repeat(MAX_COURT_NAME_CHARS + 1);
        let mut many: Vec<&str> = vec!["Court 1"; MAX_COURTS];
        many.push(&long);
        let too_many = Rejected::TooManyCourts { max: MAX_COURTS };

        let by_name = SubscriptionRequest {
            courts: names(&many),
            ..request()
        };
        assert_eq!(rejection(&registry, by_name), too_many);

        let by_id = SubscriptionRequest {
            courts: ids(&[1; MAX_COURTS + 1]),
            ..request()
        };
        assert_eq!(rejection(&registry, by_id), too_many);
    }

    #[test]
    fn a_long_name_is_rejected_ahead_of_catalogs_that_are_not_ready() {
        let registry = registry_of(vec![(tennis("north"), None)]);
        let long = "x".repeat(MAX_COURT_NAME_CHARS + 1);
        let reply = rejection(
            &registry,
            SubscriptionRequest {
                courts: names(&[&long]),
                ..request()
            },
        );
        assert_eq!(
            reply,
            Rejected::CourtNameTooLong {
                max: MAX_COURT_NAME_CHARS
            }
        );
    }

    #[test]
    fn the_input_limits_accept_a_request_exactly_at_them() {
        let registry = two_clubs();
        let sub = accepted(
            &registry,
            SubscriptionRequest {
                courts: names(&["Court 1"; MAX_COURTS]),
                ..request()
            },
        );
        assert_eq!(selected_ids(&sub), [("north", 1), ("south", 4)]);

        let longest = "é".repeat(MAX_COURT_NAME_CHARS);
        let reply = rejection(
            &registry,
            SubscriptionRequest {
                courts: names(&[&longest]),
                ..request()
            },
        );
        assert_eq!(
            reply,
            Rejected::UnknownCourts {
                unknown: names(&[&longest]).unwrap()
            },
            "the length is counted in characters, and this one is within it"
        );
    }

    #[test]
    fn one_name_can_select_more_courts_than_a_request_may_name() {
        let courts = (1..=MAX_COURTS as u128 + 5)
            .map(|id| clay(id, "Centre Court"))
            .collect();
        let registry = registry_of(vec![(tennis("north"), Some(courts))]);

        let sub = accepted(
            &registry,
            SubscriptionRequest {
                courts: names(&["Centre Court"]),
                ..request()
            },
        );

        assert_eq!(selected_ids(&sub).len(), MAX_COURTS + 5);
    }

    #[test]
    fn names_wait_for_every_catalog_in_scope_ahead_of_resolving() {
        let registry = registry_of(vec![
            (tennis("north"), Some(vec![clay(1, "Court 1")])),
            (tennis("south"), None),
        ]);
        let reply = rejection(
            &registry,
            SubscriptionRequest {
                courts: names(&["Not a court"]),
                ..request()
            },
        );
        assert_eq!(reply, Rejected::CourtCatalogsNotReady);

        let sub = accepted(
            &registry,
            SubscriptionRequest {
                venue: north(),
                courts: names(&["Court 1"]),
                ..request()
            },
        );
        assert_eq!(selected_ids(&sub), [("north", 1)]);
    }

    #[test]
    fn ids_need_a_club_because_they_are_only_unique_within_one() {
        let registry = two_clubs();
        let reply = rejection(
            &registry,
            SubscriptionRequest {
                courts: ids(&[1]),
                ..request()
            },
        );
        assert_eq!(reply, Rejected::CourtsNeedClub);
    }

    #[test]
    fn ids_at_a_club_with_no_catalog_are_unknown_rather_than_awaited() {
        let registry = registry_of(vec![(tennis("north"), None)]);
        let reply = rejection(
            &registry,
            SubscriptionRequest {
                venue: north(),
                courts: ids(&[1]),
                ..request()
            },
        );
        assert_eq!(
            reply,
            Rejected::UnknownCourts {
                unknown: ids(&[1]).unwrap()
            }
        );
    }

    #[test]
    fn only_the_rejected_part_of_a_choice_is_reported_unknown() {
        let registry = two_clubs();
        let by_name = rejection(
            &registry,
            SubscriptionRequest {
                courts: names(&["Court 2", "Court 42"]),
                ..request()
            },
        );
        assert_eq!(
            by_name,
            Rejected::UnknownCourts {
                unknown: names(&["Court 42"]).unwrap()
            }
        );

        let by_id = rejection(
            &registry,
            SubscriptionRequest {
                venue: north(),
                courts: ids(&[2, 4]),
                ..request()
            },
        );
        assert_eq!(
            by_id,
            Rejected::UnknownCourts {
                unknown: ids(&[4]).unwrap()
            },
            "south's court is not at north"
        );
    }

    #[test]
    fn unknown_courts_are_rejected_ahead_of_a_filter_excluding_the_known_ones() {
        let registry = two_clubs();
        let reply = rejection(
            &registry,
            SubscriptionRequest {
                venue: north(),
                courts: ids(&[3, 99]),
                filter: Some(CourtFilter::CLAY),
                ..request()
            },
        );
        assert_eq!(
            reply,
            Rejected::UnknownCourts {
                unknown: ids(&[99]).unwrap()
            }
        );
    }

    #[test]
    fn a_filter_excluding_a_chosen_court_is_rejected_with_its_name() {
        let registry = two_clubs();
        let reply = rejection(
            &registry,
            SubscriptionRequest {
                venue: north(),
                courts: ids(&[2, 3]),
                filter: Some(CourtFilter::CLAY),
                ..request()
            },
        );
        assert_eq!(
            reply,
            Rejected::FilterExcludesCourts {
                courts: vec!["Court 3".into()],
                filter: CourtFilter::CLAY,
            }
        );
    }

    #[test]
    fn a_name_resolves_exactly_before_it_falls_back_to_its_number() {
        let registry = two_clubs();
        let selecting = |choice: &[&str], venue| {
            selected_ids(&accepted(
                &registry,
                SubscriptionRequest {
                    venue,
                    courts: names(choice),
                    ..request()
                },
            ))
            .into_iter()
            .map(|(_, id)| id)
            .collect::<Vec<u128>>()
        };

        assert_eq!(selecting(&["court 1"], None), [1, 4]);
        assert_eq!(selecting(&["  Court 1  "], north()), [1]);
        assert_eq!(selecting(&["Pista 2"], None), [5]);
        assert_eq!(selecting(&["2"], None), [2, 5]);
    }

    #[test]
    fn a_court_chosen_twice_is_selected_once() {
        let registry = two_clubs();
        let by_id = accepted(
            &registry,
            SubscriptionRequest {
                venue: north(),
                courts: ids(&[1, 2, 1]),
                ..request()
            },
        );
        assert_eq!(selected_ids(&by_id), [("north", 1), ("north", 2)]);

        let by_name = accepted(
            &registry,
            SubscriptionRequest {
                venue: north(),
                courts: names(&["2", "Court 2"]),
                ..request()
            },
        );
        assert_eq!(selected_ids(&by_name), [("north", 2)]);
    }

    #[test]
    fn no_courts_or_an_empty_choice_means_every_court_and_the_default_filter() {
        let registry = two_clubs();
        for courts in [None, names(&[]), ids(&[])] {
            let sub = accepted(
                &registry,
                SubscriptionRequest {
                    courts,
                    ..request()
                },
            );
            assert_eq!(sub.courts, CourtSelection::Every);
            assert_eq!(sub.filter, CourtFilter::CLAY);
        }
    }

    #[test]
    fn naming_courts_or_another_sport_passes_over_the_tennis_default() {
        let registry = registry_of(vec![
            (tennis("north"), Some(vec![synthetic(3, "Court 3")])),
            (padel("casa"), Some(vec![])),
        ]);

        let chosen = accepted(
            &registry,
            SubscriptionRequest {
                courts: names(&["Court 3"]),
                ..request()
            },
        );
        assert_eq!(chosen.filter, CourtFilter::Any);

        let padel_sub = accepted(
            &registry,
            SubscriptionRequest {
                sport: Sport::Padel,
                ..request()
            },
        );
        assert_eq!(padel_sub.filter, CourtFilter::Any);
    }

    #[test]
    fn an_explicit_filter_beats_the_default() {
        let registry = two_clubs();
        let filter = CourtFilter::Surface(CourtSurface::Synthetic);
        let sub = accepted(
            &registry,
            SubscriptionRequest {
                filter: Some(filter),
                ..request()
            },
        );
        assert_eq!(sub.filter, filter);
    }

    #[test]
    fn stored_names_resolve_exactly_and_report_what_is_left_over() {
        let registry = two_clubs();
        let catalogs = registry.ready_catalogs(Sport::Tennis, None).unwrap();
        let stored = |names: &[&str]| -> Vec<String> {
            names.iter().map(|name| (*name).to_owned()).collect()
        };

        let found = courts_with_stored_names(&catalogs, &stored(&["Court 1", "COURT 1"])).unwrap();
        assert_eq!(found.len(), 2, "north's and south's, once each");

        assert_eq!(
            courts_with_stored_names(&catalogs, &stored(&["2", "Court 1", "Gone"])),
            Err(stored(&["2", "Gone"])),
            "number shorthand does not apply"
        );
    }
}
