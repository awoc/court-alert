use anyhow::Result;
use tracing::warn;

use crate::model::{
    BookableSlot, CourtSelection, SubscriberRef, Subscription, SubscriptionDraft,
    SubscriptionPolicy, SubscriptionRejection, SubscriptionRequest, VenueRegistry,
};
use crate::subscriptions::contract::{
    AvailableSlotSummary, ClubRef, NotAuthorized, OwnedSubscriptionSummary, SelectedCourtSummary,
    Subscribed, SubscriptionSummary, UnsubscribeOutcome,
};

use super::matcher::slot_matches;
use super::{SubscriptionService, slot_summary};

impl SubscriptionService {
    pub async fn subscribe(
        &self,
        subscriber: &SubscriberRef,
        request: &SubscriptionRequest,
    ) -> Result<Result<Subscribed, SubscriptionRejection>> {
        let draft = match self.validate_subscribe(subscriber, request) {
            Ok(draft) => draft,
            Err(rejection) => return Ok(Err(rejection)),
        };

        let id = self.store.add(draft.clone()).await?;
        let sub = draft.into_subscription(id);
        let open_slots = self.open_slots_for(&sub).await;
        Ok(Ok(Subscribed {
            summary: self.summarize(sub),
            open_slots,
        }))
    }

    pub async fn list(&self, subscriber: &SubscriberRef) -> Result<Vec<SubscriptionSummary>> {
        let subs = self
            .store
            .list_for_subscriber(subscriber, self.clock.today())
            .await?;
        Ok(subs.into_iter().map(|sub| self.summarize(sub)).collect())
    }

    pub async fn unsubscribe(
        &self,
        subscriber: &SubscriberRef,
        id: i64,
    ) -> Result<UnsubscribeOutcome> {
        let removed = if self.is_admin(subscriber) {
            self.store.remove_any(id).await?
        } else {
            self.store.remove(id, subscriber).await?
        };
        Ok(if removed {
            UnsubscribeOutcome::Removed { id }
        } else {
            UnsubscribeOutcome::NotFound { id }
        })
    }

    pub async fn list_all(
        &self,
        subscriber: &SubscriberRef,
    ) -> Result<Result<Vec<OwnedSubscriptionSummary>, NotAuthorized>> {
        if !self.is_admin(subscriber) {
            return Ok(Err(NotAuthorized));
        }
        let subs = self.store.list_all(self.clock.today()).await?;
        Ok(Ok(subs
            .into_iter()
            .map(|sub| OwnedSubscriptionSummary {
                subscriber: sub.subscriber.clone(),
                summary: self.summarize(sub),
            })
            .collect()))
    }

    fn validate_subscribe(
        &self,
        subscriber: &SubscriberRef,
        request: &SubscriptionRequest,
    ) -> Result<SubscriptionDraft, SubscriptionRejection> {
        let registry = self.registry.read().expect("venue registry poisoned");
        SubscriptionPolicy {
            registry: &registry,
            tennis_default_filter: self.tennis_default_filter,
            today: self.clock.today(),
        }
        .draft(subscriber, request)
    }

    fn summarize(&self, sub: Subscription) -> SubscriptionSummary {
        let registry = self.registry.read().expect("venue registry poisoned");
        let club = sub.venue.map(|venue_id| ClubRef {
            name: registry.club_label(&venue_id),
            venue_id,
        });
        SubscriptionSummary {
            id: sub.id,
            sport: sub.sport,
            club,
            schedule: sub.schedule,
            time_range: sub.time_range,
            courts: selected_court_summaries(&registry, sub.courts),
            filter: sub.filter,
        }
    }

    async fn open_slots_for(&self, sub: &Subscription) -> Vec<AvailableSlotSummary> {
        let snapshot = match self.slot_snapshots.load_snapshot().await {
            Ok(snapshot) => snapshot,
            Err(e) => {
                warn!(
                    error = %format!("{e:#}"),
                    "subscribe preview: loading bookable-slot snapshot failed"
                );
                return Vec::new();
            }
        };
        let now = self.clock.now();
        let registry = self.registry.read().expect("venue registry poisoned");
        let mut slots: Vec<&BookableSlot> = snapshot
            .values()
            .filter(|slot| slot.starts_at > now && slot_matches(sub, slot, &registry))
            .collect();
        slots.sort_by_key(|slot| slot.starts_at);
        slots
            .into_iter()
            .map(|slot| slot_summary(&registry, slot))
            .collect()
    }
}

fn selected_court_summaries(
    registry: &VenueRegistry,
    courts: CourtSelection,
) -> Option<Vec<SelectedCourtSummary>> {
    match courts {
        CourtSelection::Every => None,
        CourtSelection::Selected(courts) => Some(
            courts
                .into_iter()
                .map(|court| SelectedCourtSummary::Identified {
                    id: court.court_id,
                    name: registry.court_name(&court.venue_id, court.court_id),
                })
                .collect(),
        ),
        CourtSelection::PendingNames(names) => Some(
            names
                .into_iter()
                .map(SelectedCourtSummary::PendingName)
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::super::SubscriptionService;
    use super::super::testing::{
        PADEL_CLUB_NAME, SYNTHETIC_COURT, admin_uref, clay_court, court_id, date_subscribe_request,
        open_slot, padel_club, padel_court, padel_venue_id, registry_of, service,
        service_defaulting_to_clay, service_on, service_over, service_with_admin,
        service_with_clock, service_with_failing_slot_snapshot, service_with_slots,
        service_with_store, service_without_padel, store, subscribe_request, subscribe_to_courts,
        tennis_club, uref, venue_id,
    };
    use crate::model::{
        Court, CourtAttributes, CourtCatalog, CourtFilter, CourtLocation, CourtSelection,
        CourtSurface, Schedule, SelectedCourt, Sport, TimeRange, UncheckedDraft, VenueId,
    };
    use crate::ports::SubscriptionRepository;
    use crate::subscriptions::contract::{
        ClubRef, CourtChoice, NotAuthorized, SelectedCourtSummary, Subscribed,
        SubscriptionRejection, SubscriptionRequest, SubscriptionSummary, UnsubscribeOutcome,
    };
    use chrono::Weekday;
    use std::sync::Arc;
    use uuid::Uuid;

    fn tennis_court(name: &str) -> SelectedCourtSummary {
        SelectedCourtSummary::Identified {
            id: court_id(name),
            name: Some(name.into()),
        }
    }

    fn court_ids(summary_courts: Option<Vec<SelectedCourtSummary>>) -> Vec<u128> {
        summary_courts
            .unwrap()
            .into_iter()
            .map(|court| match court {
                SelectedCourtSummary::Identified { id, .. } => id.as_u128(),
                SelectedCourtSummary::PendingName(name) => panic!("{name} has no id"),
            })
            .collect()
    }

    async fn subscribed_courts(
        svc: &SubscriptionService,
        sport: Sport,
        venue: Option<&VenueId>,
        courts: CourtChoice,
    ) -> Vec<u128> {
        let reply = svc
            .subscribe(&uref("1"), &subscribe_to_courts(sport, venue, courts))
            .await
            .unwrap();
        let Subscribed { summary, .. } = reply.unwrap();
        court_ids(summary.courts)
    }

    fn names(names: &[&str]) -> CourtChoice {
        CourtChoice::Names(names.iter().map(|name| (*name).to_owned()).collect())
    }

    fn ids(ids: &[u128]) -> CourtChoice {
        CourtChoice::Ids(ids.iter().map(|id| Uuid::from_u128(*id)).collect())
    }

    /// Clubs `north` and `south` share "Court 2". South also has "Pista 2".
    async fn two_tennis_clubs() -> Arc<SubscriptionService> {
        service_over(registry_of(vec![
            (
                tennis_club("north"),
                Some(vec![clay_court(1, "Court 2"), clay_court(2, "Court 3")]),
            ),
            (
                tennis_club("south"),
                Some(vec![clay_court(3, "Court 2"), clay_court(4, "Pista 2")]),
            ),
        ]))
        .await
    }

    #[tokio::test]
    async fn a_name_with_no_club_selects_every_court_of_that_name() {
        let svc = two_tennis_clubs().await;
        let chosen = subscribed_courts(&svc, Sport::Tennis, None, names(&["court 2"])).await;
        assert_eq!(chosen, [1, 3]);
    }

    #[tokio::test]
    async fn a_number_with_no_club_selects_every_court_with_that_number() {
        let svc = two_tennis_clubs().await;
        let chosen = subscribed_courts(&svc, Sport::Tennis, None, names(&["2"])).await;
        assert_eq!(chosen, [1, 3, 4]);
    }

    #[tokio::test]
    async fn a_name_with_a_club_stays_within_that_club() {
        let svc = two_tennis_clubs().await;
        let south = VenueId::new("south");

        let by_name = subscribed_courts(&svc, Sport::Tennis, Some(&south), names(&["Court 2"]));
        assert_eq!(by_name.await, [3]);

        let by_number = subscribed_courts(&svc, Sport::Tennis, Some(&south), names(&["2"]));
        assert_eq!(by_number.await, [3, 4]);
    }

    #[tokio::test]
    async fn a_court_called_exactly_that_beats_a_number_the_name_contains() {
        let svc = two_tennis_clubs().await;
        let chosen = subscribed_courts(&svc, Sport::Tennis, None, names(&["Pista 2"])).await;
        assert_eq!(chosen, [4]);
    }

    #[tokio::test]
    async fn a_name_cannot_resolve_while_a_club_in_scope_has_no_catalog_yet() {
        let svc = service_over(registry_of(vec![
            (tennis_club("north"), Some(vec![clay_court(1, "Court 2")])),
            (tennis_club("south"), None),
        ]))
        .await;

        let reply = svc
            .subscribe(
                &uref("1"),
                &subscribe_to_courts(Sport::Tennis, None, names(&["Court 2"])),
            )
            .await
            .unwrap();
        assert_eq!(reply, Err(SubscriptionRejection::CourtCatalogsNotReady));
        assert!(svc.list(&uref("1")).await.unwrap().is_empty());

        let north = VenueId::new("north");
        let chosen = subscribed_courts(&svc, Sport::Tennis, Some(&north), names(&["Court 2"]));
        assert_eq!(chosen.await, [1]);
    }

    #[tokio::test]
    async fn court_ids_are_deduplicated_by_identity_rather_than_by_name() {
        let club = VenueId::new("club");
        let svc = service_over(registry_of(vec![(
            padel_club("club"),
            Some(vec![
                padel_court(1, "Centre Court"),
                padel_court(2, "Centre Court"),
            ]),
        )]))
        .await;

        let chosen = subscribed_courts(&svc, Sport::Padel, Some(&club), ids(&[1, 2, 1])).await;

        assert_eq!(chosen, [1, 2]);
    }

    #[tokio::test]
    async fn a_filter_checks_the_chosen_court_rather_than_its_namesake() {
        let club = VenueId::new("club");
        let svc = service_over(registry_of(vec![(
            tennis_club("club"),
            Some(vec![
                clay_court(1, "Centre Court"),
                Court::new(
                    Uuid::from_u128(2),
                    "Centre Court".into(),
                    CourtAttributes::tennis(CourtSurface::Synthetic),
                ),
            ]),
        )]))
        .await;
        let choosing = |court: u128| SubscriptionRequest {
            sport: Sport::Tennis,
            venue: Some(club.clone()),
            schedule: Schedule::Weekday(Weekday::Tue),
            start_minute: 0,
            end_minute: 24 * 60,
            courts: Some(ids(&[court])),
            filter: Some(CourtFilter::CLAY),
        };

        let clay = svc.subscribe(&uref("1"), &choosing(1)).await.unwrap();
        assert!(clay.is_ok());

        let synthetic = svc.subscribe(&uref("1"), &choosing(2)).await.unwrap();
        assert_eq!(
            synthetic,
            Err(SubscriptionRejection::FilterExcludesCourts {
                courts: vec!["Centre Court".to_string()],
                filter: CourtFilter::CLAY,
            })
        );
    }

    #[tokio::test]
    async fn a_filter_rejects_a_name_when_any_of_its_courts_is_excluded() {
        let svc = service_over(registry_of(vec![
            (tennis_club("north"), Some(vec![clay_court(1, "Court 2")])),
            (
                tennis_club("south"),
                Some(vec![Court::new(
                    Uuid::from_u128(2),
                    "Court 2".into(),
                    CourtAttributes::tennis(CourtSurface::Synthetic),
                )]),
            ),
        ]))
        .await;

        let reply = svc
            .subscribe(
                &uref("1"),
                &SubscriptionRequest {
                    sport: Sport::Tennis,
                    venue: None,
                    schedule: Schedule::Weekday(Weekday::Tue),
                    start_minute: 0,
                    end_minute: 24 * 60,
                    courts: Some(names(&["Court 2"])),
                    filter: Some(CourtFilter::CLAY),
                },
            )
            .await
            .unwrap();

        assert_eq!(
            reply,
            Err(SubscriptionRejection::FilterExcludesCourts {
                courts: vec!["Court 2".to_string()],
                filter: CourtFilter::CLAY,
            })
        );
    }

    fn listed_courts(mut subs: Vec<SubscriptionSummary>) -> Vec<SelectedCourtSummary> {
        subs.remove(0).courts.unwrap()
    }

    #[tokio::test]
    async fn listing_takes_court_names_from_the_current_catalog() {
        let club = VenueId::new("club");
        let registry = registry_of(vec![(
            padel_club("club"),
            Some(vec![padel_court(1, "Centre Court")]),
        )]);
        let svc = service_over(registry.clone()).await;
        subscribed_courts(&svc, Sport::Padel, Some(&club), ids(&[1])).await;

        registry
            .write()
            .unwrap()
            .set_catalog(&club, CourtCatalog::new(vec![padel_court(1, "Show Court")]));
        assert_eq!(
            listed_courts(svc.list(&uref("1")).await.unwrap()),
            [SelectedCourtSummary::Identified {
                id: Uuid::from_u128(1),
                name: Some("Show Court".into()),
            }]
        );

        registry
            .write()
            .unwrap()
            .set_catalog(&club, CourtCatalog::default());
        assert_eq!(
            listed_courts(svc.list(&uref("1")).await.unwrap()),
            [SelectedCourtSummary::Identified {
                id: Uuid::from_u128(1),
                name: None,
            }],
            "a court that left the catalog stays listed"
        );
    }

    #[tokio::test]
    async fn listing_keeps_a_selection_whose_catalog_is_not_ready() {
        let store = store().await;
        store
            .add(
                UncheckedDraft {
                    subscriber: uref("1"),
                    sport: Sport::Padel,
                    venue: Some(VenueId::new("club")),
                    schedule: Schedule::Weekday(Weekday::Tue),
                    time_range: TimeRange::new(0, 24 * 60).unwrap(),
                    courts: CourtSelection::Selected(vec![SelectedCourt {
                        venue_id: VenueId::new("club"),
                        court_id: Uuid::from_u128(1),
                    }]),
                    filter: CourtFilter::Any,
                }
                .assume_valid(),
            )
            .await
            .unwrap();
        let svc = service_on(store, registry_of(vec![(padel_club("club"), None)]));

        assert_eq!(
            listed_courts(svc.list(&uref("1")).await.unwrap()),
            [SelectedCourtSummary::Identified {
                id: Uuid::from_u128(1),
                name: None,
            }]
        );
    }

    #[tokio::test]
    async fn listing_shows_a_pending_selection_by_its_stored_name() {
        let (svc, store) = service_with_store().await;
        store
            .add(
                UncheckedDraft {
                    subscriber: uref("1"),
                    sport: Sport::Tennis,
                    venue: None,
                    schedule: Schedule::Weekday(Weekday::Tue),
                    time_range: TimeRange::new(0, 24 * 60).unwrap(),
                    courts: CourtSelection::PendingNames(vec!["Court 2".into()]),
                    filter: CourtFilter::Any,
                }
                .assume_valid(),
            )
            .await
            .unwrap();

        assert_eq!(
            listed_courts(svc.list(&uref("1")).await.unwrap()),
            [SelectedCourtSummary::PendingName("Court 2".into())]
        );
    }

    #[tokio::test]
    async fn listing_keeps_a_row_the_policy_would_refuse_today() {
        let (svc, store) = service_with_store().await;
        let long_name = "x".repeat(101);
        let many_names = vec![long_name.clone(); 25];
        store
            .add(
                UncheckedDraft {
                    subscriber: uref("1"),
                    sport: Sport::Padel,
                    venue: Some(VenueId::new("closed-club")),
                    schedule: Schedule::Weekday(Weekday::Tue),
                    time_range: TimeRange::new(0, 24 * 60).unwrap(),
                    courts: CourtSelection::PendingNames(many_names),
                    filter: CourtFilter::CLAY,
                }
                .assume_valid(),
            )
            .await
            .unwrap();

        let subs = svc.list(&uref("1")).await.unwrap();

        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].filter, CourtFilter::CLAY);
        assert_eq!(
            subs[0].courts,
            Some(vec![SelectedCourtSummary::PendingName(long_name); 25])
        );
    }

    #[tokio::test]
    async fn subscribe_persists_and_returns_summary() {
        let svc = service().await;
        let reply = svc
            .subscribe(&uref("1"), &subscribe_request(18 * 60, 20 * 60))
            .await
            .unwrap();
        let Subscribed {
            summary,
            open_slots,
        } = reply.unwrap();
        assert_eq!(summary.schedule, Schedule::Weekday(Weekday::Tue));
        assert_eq!(summary.time_range, TimeRange::new(1080, 1200).unwrap());
        assert!(open_slots.is_empty()); // empty snapshot → empty preview

        let subs = svc.list(&uref("1")).await.unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].id, summary.id);
    }

    #[tokio::test]
    async fn subscribe_rejects_inverted_time_range_and_stores_nothing() {
        let svc = service().await;
        let reply = svc
            .subscribe(&uref("1"), &subscribe_request(20 * 60, 18 * 60))
            .await
            .unwrap();
        assert_eq!(reply, Err(SubscriptionRejection::InvalidTimeRange));

        let list = svc.list(&uref("1")).await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn list_returns_only_own_subscriptions() {
        let svc = service().await;
        svc.subscribe(&uref("1"), &subscribe_request(18 * 60, 20 * 60))
            .await
            .unwrap()
            .unwrap();
        svc.subscribe(&uref("2"), &subscribe_request(8 * 60, 10 * 60))
            .await
            .unwrap()
            .unwrap();

        let subs = svc.list(&uref("1")).await.unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].time_range.start_minute(), 18 * 60);
    }

    #[tokio::test]
    async fn list_filters_expired_one_shot_subscriptions_without_cleanup_write() {
        let (svc, store) = super::super::testing::service_with_store().await;
        store
            .add(
                UncheckedDraft {
                    subscriber: uref("1"),
                    sport: Sport::Tennis,
                    venue: None,
                    schedule: Schedule::Date(chrono::NaiveDate::from_ymd_opt(2000, 1, 1).unwrap()),
                    time_range: TimeRange::new(18 * 60, 20 * 60).unwrap(),
                    courts: CourtSelection::Every,
                    filter: CourtFilter::Any,
                }
                .assume_valid(),
            )
            .await
            .unwrap();

        let reply = svc.list(&uref("1")).await.unwrap();

        assert!(reply.is_empty());
        let before_expiry = chrono::NaiveDate::from_ymd_opt(1900, 1, 1).unwrap();
        assert_eq!(store.list_all(before_expiry).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unsubscribe_removes_own_and_rejects_foreign() {
        let svc = service().await;
        let Subscribed { summary, .. } = svc
            .subscribe(&uref("1"), &subscribe_request(18 * 60, 20 * 60))
            .await
            .unwrap()
            .unwrap();

        let foreign = svc.unsubscribe(&uref("2"), summary.id).await.unwrap();
        assert_eq!(foreign, UnsubscribeOutcome::NotFound { id: summary.id });

        let own = svc.unsubscribe(&uref("1"), summary.id).await.unwrap();
        assert_eq!(own, UnsubscribeOutcome::Removed { id: summary.id });
    }

    #[tokio::test]
    async fn subscribe_rejects_equal_start_and_end() {
        let svc = service().await;
        let reply = svc
            .subscribe(&uref("1"), &subscribe_request(18 * 60, 18 * 60))
            .await
            .unwrap();
        assert_eq!(reply, Err(SubscriptionRejection::InvalidTimeRange));
    }

    #[tokio::test]
    async fn subscribe_rejects_time_range_outside_one_day() {
        let svc = service().await;
        let reply = svc
            .subscribe(&uref("1"), &subscribe_request(23 * 60, 25 * 60))
            .await
            .unwrap();
        assert_eq!(reply, Err(SubscriptionRejection::InvalidTimeRange));
    }

    #[tokio::test]
    async fn subscribe_rejects_past_date() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 15).unwrap();
        let svc = service_with_clock(today).await;
        let yesterday = today.pred_opt().unwrap();
        let reply = svc
            .subscribe(
                &uref("1"),
                &date_subscribe_request(yesterday, 18 * 60, 20 * 60),
            )
            .await
            .unwrap();
        assert_eq!(reply, Err(SubscriptionRejection::InvalidSchedule));
        assert!(svc.list(&uref("1")).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn subscribe_accepts_todays_date() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 15).unwrap();
        let svc = service_with_clock(today).await;
        let reply = svc
            .subscribe(&uref("1"), &date_subscribe_request(today, 18 * 60, 20 * 60))
            .await
            .unwrap();
        assert!(reply.is_ok());
    }

    #[tokio::test]
    async fn subscribe_accepts_far_future_date() {
        let svc = service().await;
        let far = chrono::NaiveDate::from_ymd_opt(2099, 12, 31).unwrap();
        let reply = svc
            .subscribe(&uref("1"), &date_subscribe_request(far, 18 * 60, 20 * 60))
            .await
            .unwrap();
        assert!(reply.is_ok());
    }

    #[tokio::test]
    async fn listall_returns_everything_for_admin() {
        let svc = service_with_admin().await;
        svc.subscribe(&uref("1"), &subscribe_request(18 * 60, 20 * 60))
            .await
            .unwrap()
            .unwrap();
        svc.subscribe(&uref("2"), &subscribe_request(8 * 60, 10 * 60))
            .await
            .unwrap()
            .unwrap();

        let all = svc.list_all(&admin_uref()).await.unwrap().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn listall_denied_for_non_admin() {
        let svc = service_with_admin().await;
        let reply = svc.list_all(&uref("1")).await.unwrap();
        assert_eq!(reply, Err(NotAuthorized));
    }

    #[tokio::test]
    async fn admin_can_unsubscribe_foreign_subscription() {
        let svc = service_with_admin().await;
        let Subscribed { summary: s, .. } = svc
            .subscribe(&uref("1"), &subscribe_request(18 * 60, 20 * 60))
            .await
            .unwrap()
            .unwrap();
        let reply = svc.unsubscribe(&admin_uref(), s.id).await.unwrap();
        assert_eq!(reply, UnsubscribeOutcome::Removed { id: s.id });
    }

    #[tokio::test]
    async fn non_admin_cannot_unsubscribe_foreign_subscription() {
        let svc = service_with_admin().await;
        let Subscribed { summary: s, .. } = svc
            .subscribe(&uref("1"), &subscribe_request(18 * 60, 20 * 60))
            .await
            .unwrap()
            .unwrap();
        let reply = svc.unsubscribe(&uref("2"), s.id).await.unwrap();
        assert_eq!(reply, UnsubscribeOutcome::NotFound { id: s.id });
    }

    #[tokio::test]
    async fn subscribe_passes_courts_through_to_summary() {
        let svc = service().await;
        let reply = svc
            .subscribe(
                &uref("1"),
                &SubscriptionRequest {
                    sport: Sport::Tennis,
                    venue: None,
                    schedule: Schedule::Weekday(chrono::Weekday::Tue),
                    start_minute: 18 * 60,
                    end_minute: 20 * 60,
                    courts: Some(CourtChoice::Names(vec!["Court 2".into()])),
                    filter: None,
                },
            )
            .await
            .unwrap();
        let Subscribed { summary, .. } = reply.unwrap();
        assert_eq!(summary.courts, Some(vec![tennis_court("Court 2")]));
    }

    #[tokio::test]
    async fn subscribe_canonicalizes_courts_by_number() {
        let svc = service().await;
        for input in ["court 2", "2", "Cort 2"] {
            let reply = svc
                .subscribe(
                    &uref("1"),
                    &SubscriptionRequest {
                        sport: Sport::Tennis,
                        venue: None,
                        schedule: Schedule::Weekday(chrono::Weekday::Tue),
                        start_minute: 18 * 60,
                        end_minute: 20 * 60,
                        courts: Some(CourtChoice::Names(vec![input.into()])),
                        filter: None,
                    },
                )
                .await
                .unwrap();
            let Subscribed { summary, .. } = reply.unwrap();
            assert_eq!(summary.courts, Some(vec![tennis_court("Court 2")]));
        }
    }

    fn courts_request(courts: Vec<String>) -> SubscriptionRequest {
        SubscriptionRequest {
            sport: Sport::Tennis,
            venue: None,
            schedule: Schedule::Weekday(Weekday::Tue),
            start_minute: 18 * 60,
            end_minute: 20 * 60,
            courts: Some(CourtChoice::Names(courts)),
            filter: None,
        }
    }

    #[tokio::test]
    async fn too_many_or_overlong_court_names_are_rejected_and_store_nothing() {
        let svc = service().await;
        let too_many = vec!["Court 2".to_string(); 21];
        assert_eq!(
            svc.subscribe(&uref("1"), &courts_request(too_many))
                .await
                .unwrap(),
            Err(SubscriptionRejection::TooManyCourts { max: 20 })
        );
        assert_eq!(
            svc.subscribe(&uref("1"), &courts_request(vec!["x".repeat(101)]))
                .await
                .unwrap(),
            Err(SubscriptionRejection::CourtNameTooLong { max: 100 })
        );
        assert!(svc.list(&uref("1")).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn one_name_can_store_more_courts_than_a_command_may_name() {
        let courts = (1..=25).map(|id| clay_court(id, "Centre Court")).collect();
        let svc = service_over(registry_of(vec![(tennis_club("club"), Some(courts))])).await;

        let chosen = subscribed_courts(&svc, Sport::Tennis, None, names(&["Centre Court"])).await;
        assert_eq!(chosen.len(), 25);

        assert_eq!(listed_courts(svc.list(&uref("1")).await.unwrap()).len(), 25);
    }

    #[tokio::test]
    async fn courts_named_twice_are_stored_once() {
        let svc = service().await;
        let Subscribed { summary, .. } = svc
            .subscribe(
                &uref("1"),
                &courts_request(vec!["2".into(), "Court 2".into(), "5".into()]),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            summary.courts,
            Some(vec![tennis_court("Court 2"), tennis_court("Court 5")])
        );
    }

    #[tokio::test]
    async fn an_empty_court_list_means_every_court() {
        let svc = service_defaulting_to_clay().await;
        let Subscribed { summary, .. } = svc
            .subscribe(
                &uref("1"),
                &SubscriptionRequest {
                    sport: Sport::Tennis,
                    venue: None,
                    schedule: Schedule::Weekday(Weekday::Tue),
                    start_minute: 18 * 60,
                    end_minute: 20 * 60,
                    courts: Some(CourtChoice::Names(vec![])),
                    filter: None,
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(summary.courts, None);
        assert_eq!(summary.filter, CourtFilter::CLAY);
    }

    #[tokio::test]
    async fn subscribe_rejects_unknown_courts_and_stores_nothing() {
        let svc = service().await;
        let reply = svc
            .subscribe(
                &uref("1"),
                &SubscriptionRequest {
                    sport: Sport::Tennis,
                    venue: None,
                    schedule: Schedule::Weekday(chrono::Weekday::Tue),
                    start_minute: 18 * 60,
                    end_minute: 20 * 60,
                    courts: Some(CourtChoice::Names(vec![
                        "Court 42".into(),
                        "Court 5".into(),
                    ])),
                    filter: None,
                },
            )
            .await
            .unwrap();
        let Err(SubscriptionRejection::UnknownCourts { unknown, available }) = reply else {
            panic!("expected UnknownCourts, got {reply:?}");
        };
        assert_eq!(unknown, CourtChoice::Names(vec!["Court 42".to_string()]));
        assert!(available.iter().any(|court| court.name() == "Court 2"));
        assert!(svc.list(&uref("1")).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn unknown_court_ids_are_reported_as_ids_and_store_nothing() {
        let svc = service().await;
        let stranger = uuid::Uuid::from_u128(4242);
        let reply = svc
            .subscribe(
                &uref("1"),
                &SubscriptionRequest {
                    sport: Sport::Tennis,
                    venue: Some(venue_id()),
                    schedule: Schedule::Weekday(Weekday::Tue),
                    start_minute: 18 * 60,
                    end_minute: 20 * 60,
                    courts: Some(CourtChoice::Ids(vec![court_id("Court 2"), stranger])),
                    filter: None,
                },
            )
            .await
            .unwrap();
        let Err(SubscriptionRejection::UnknownCourts { unknown, .. }) = reply else {
            panic!("expected UnknownCourts, got {reply:?}");
        };
        assert_eq!(unknown, CourtChoice::Ids(vec![stranger]));
        assert!(svc.list(&uref("1")).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn subscribe_without_courts_takes_the_configured_default_filter() {
        let svc = service_defaulting_to_clay().await;
        let Subscribed { summary, .. } = svc
            .subscribe(&uref("1"), &subscribe_request(18 * 60, 20 * 60))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(summary.filter, CourtFilter::CLAY);
    }

    #[tokio::test]
    async fn naming_courts_overrides_the_default_filter() {
        let svc = service_defaulting_to_clay().await;
        let Subscribed { summary, .. } = svc
            .subscribe(
                &uref("1"),
                &SubscriptionRequest {
                    sport: Sport::Tennis,
                    venue: None,
                    schedule: Schedule::Weekday(chrono::Weekday::Tue),
                    start_minute: 18 * 60,
                    end_minute: 20 * 60,
                    courts: Some(CourtChoice::Names(vec!["19".into()])),
                    filter: None,
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(summary.courts, Some(vec![tennis_court(SYNTHETIC_COURT)]));
        assert_eq!(summary.filter, CourtFilter::Any);
    }

    #[tokio::test]
    async fn an_explicit_filter_beats_the_configured_default() {
        let svc = service_defaulting_to_clay().await;
        let Subscribed { summary, .. } = svc
            .subscribe(
                &uref("1"),
                &SubscriptionRequest {
                    sport: Sport::Tennis,
                    venue: None,
                    schedule: Schedule::Weekday(chrono::Weekday::Tue),
                    start_minute: 18 * 60,
                    end_minute: 20 * 60,
                    courts: None,
                    filter: Some(CourtFilter::Surface(CourtSurface::Synthetic)),
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            summary.filter,
            CourtFilter::Surface(CourtSurface::Synthetic)
        );
    }

    #[tokio::test]
    async fn a_filter_contradicting_the_named_courts_is_rejected_and_stores_nothing() {
        let svc = service().await;
        let reply = svc
            .subscribe(
                &uref("1"),
                &SubscriptionRequest {
                    sport: Sport::Tennis,
                    venue: None,
                    schedule: Schedule::Weekday(chrono::Weekday::Tue),
                    start_minute: 18 * 60,
                    end_minute: 20 * 60,
                    courts: Some(CourtChoice::Names(vec!["19".into(), "2".into()])),
                    filter: Some(CourtFilter::CLAY),
                },
            )
            .await
            .unwrap();

        assert_eq!(
            reply,
            Err(SubscriptionRejection::FilterExcludesCourts {
                courts: vec![SYNTHETIC_COURT.to_string()],
                filter: CourtFilter::CLAY,
            })
        );
        assert!(svc.list(&uref("1")).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_filter_matching_the_named_courts_is_accepted() {
        let svc = service().await;
        let Subscribed { summary, .. } = svc
            .subscribe(
                &uref("1"),
                &SubscriptionRequest {
                    sport: Sport::Tennis,
                    venue: None,
                    schedule: Schedule::Weekday(chrono::Weekday::Tue),
                    start_minute: 18 * 60,
                    end_minute: 20 * 60,
                    courts: Some(CourtChoice::Names(vec!["2".into()])),
                    filter: Some(CourtFilter::CLAY),
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(summary.filter, CourtFilter::CLAY);
    }

    #[tokio::test]
    async fn a_filter_from_the_other_sports_vocabulary_is_rejected_and_stores_nothing() {
        let svc = service().await;
        let cases = [
            (Sport::Tennis, CourtFilter::Location(CourtLocation::Indoor)),
            (Sport::Padel, CourtFilter::CLAY),
        ];
        for (sport, filter) in cases {
            let reply = svc
                .subscribe(
                    &uref("1"),
                    &SubscriptionRequest {
                        sport,
                        venue: None,
                        schedule: Schedule::Weekday(Weekday::Tue),
                        start_minute: 18 * 60,
                        end_minute: 20 * 60,
                        courts: None,
                        filter: Some(filter),
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                reply,
                Err(SubscriptionRejection::FilterNotForSport { sport, filter })
            );
        }
        assert!(svc.list(&uref("1")).await.unwrap().is_empty());
    }

    fn padel_request(venue: Option<crate::model::VenueId>) -> SubscriptionRequest {
        SubscriptionRequest {
            sport: Sport::Padel,
            venue,
            schedule: Schedule::Weekday(Weekday::Tue),
            start_minute: 18 * 60,
            end_minute: 20 * 60,
            courts: None,
            filter: None,
        }
    }

    #[tokio::test]
    async fn padel_does_not_inherit_the_configured_tennis_default() {
        let svc = service_defaulting_to_clay().await;

        let Subscribed { summary, .. } = svc
            .subscribe(&uref("1"), &padel_request(None))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(summary.filter, CourtFilter::Any);
        assert_eq!(summary.sport, Sport::Padel);
    }

    #[tokio::test]
    async fn padel_records_the_club_it_was_given_and_none_for_all_clubs() {
        let svc = service().await;

        let Subscribed { summary, .. } = svc
            .subscribe(&uref("1"), &padel_request(Some(padel_venue_id())))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            summary.club,
            Some(ClubRef {
                venue_id: padel_venue_id(),
                name: PADEL_CLUB_NAME.to_string(),
            })
        );

        let Subscribed { summary, .. } = svc
            .subscribe(&uref("1"), &padel_request(None))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(summary.club, None);
    }

    #[tokio::test]
    async fn an_unknown_club_is_rejected_and_stores_nothing() {
        let svc = service().await;

        let reply = svc
            .subscribe(
                &uref("1"),
                &padel_request(Some(crate::model::VenueId::new("not-a-club"))),
            )
            .await
            .unwrap();

        let Err(SubscriptionRejection::UnknownClub { unknown, available }) = reply else {
            panic!("expected UnknownClub, got {reply:?}");
        };
        assert_eq!(unknown, VenueId::new("not-a-club"));
        assert_eq!(
            available,
            vec![ClubRef {
                venue_id: padel_venue_id(),
                name: PADEL_CLUB_NAME.to_string(),
            }]
        );
        assert!(svc.list(&uref("1")).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn padel_with_no_configured_club_says_so_and_stores_nothing() {
        let svc = service_without_padel().await;

        let reply = svc
            .subscribe(&uref("1"), &padel_request(None))
            .await
            .unwrap();

        assert_eq!(
            reply,
            Err(SubscriptionRejection::NoClubsConfigured {
                sport: Sport::Padel
            })
        );
        assert!(svc.list(&uref("1")).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn tennis_still_takes_the_configured_default() {
        let svc = service_defaulting_to_clay().await;

        let Subscribed { summary, .. } = svc
            .subscribe(&uref("1"), &subscribe_request(18 * 60, 20 * 60))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(summary.filter, CourtFilter::CLAY);
        assert_eq!(summary.sport, Sport::Tennis);
    }

    use chrono::TimeZone as _;
    use chrono::Utc;

    fn utc(day: u32, hour: u32, minute: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, day, hour, minute, 0).unwrap()
    }

    #[tokio::test]
    async fn subscribe_previews_matching_open_slots_sorted_by_start() {
        let svc = service_with_slots(
            utc(1, 12, 0),
            vec![
                open_slot("Court 2", utc(2, 17, 0)), // Tue 19:00 local — matches
                open_slot("Court 5", utc(2, 16, 0)), // Tue 18:00 local — matches
                open_slot("Court 2", utc(2, 18, 0)), // Tue 20:00 local — outside window
                open_slot("Court 2", utc(3, 16, 0)), // Wednesday — wrong weekday
            ],
        )
        .await;

        let reply = svc
            .subscribe(&uref("1"), &subscribe_request(18 * 60, 20 * 60))
            .await
            .unwrap();

        let Subscribed { open_slots, .. } = reply.unwrap();
        assert_eq!(open_slots.len(), 2);
        assert_eq!(open_slots[0].court, "Court 5"); // 18:00 sorts first
        assert_eq!(open_slots[1].court, "Court 2");
    }

    #[tokio::test]
    async fn subscribe_preview_respects_court_filter() {
        let svc = service_with_slots(
            utc(1, 12, 0),
            vec![
                open_slot("Court 2", utc(2, 16, 0)),
                open_slot("Court 5", utc(2, 16, 0)),
            ],
        )
        .await;

        let reply = svc
            .subscribe(
                &uref("1"),
                &SubscriptionRequest {
                    sport: Sport::Tennis,
                    venue: None,
                    schedule: Schedule::Weekday(chrono::Weekday::Tue),
                    start_minute: 18 * 60,
                    end_minute: 20 * 60,
                    courts: Some(CourtChoice::Names(vec!["Court 5".into()])),
                    filter: None,
                },
            )
            .await
            .unwrap();

        let Subscribed { open_slots, .. } = reply.unwrap();
        assert_eq!(open_slots.len(), 1);
        assert_eq!(open_slots[0].court, "Court 5");
    }

    #[tokio::test]
    async fn subscribe_preview_excludes_slots_already_started() {
        let svc = service_with_slots(
            utc(2, 16, 30),
            vec![
                open_slot("Court 2", utc(2, 16, 0)),
                open_slot("Court 5", utc(2, 17, 0)),
            ],
        )
        .await;

        let reply = svc
            .subscribe(&uref("1"), &subscribe_request(18 * 60, 20 * 60))
            .await
            .unwrap();

        let Subscribed { open_slots, .. } = reply.unwrap();
        assert_eq!(open_slots.len(), 1);
        assert_eq!(open_slots[0].court, "Court 5");
    }

    #[tokio::test]
    async fn subscribe_survives_slot_snapshot_failure_with_empty_preview() {
        let svc = service_with_failing_slot_snapshot().await;

        let reply = svc
            .subscribe(&uref("1"), &subscribe_request(18 * 60, 20 * 60))
            .await
            .unwrap();

        let Subscribed { open_slots, .. } = reply.unwrap();
        assert!(open_slots.is_empty());

        let subs = svc.list(&uref("1")).await.unwrap();
        assert_eq!(subs.len(), 1);
    }
}
