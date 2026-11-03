use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use tracing::{info, warn};

use crate::model::{
    CourtSelection, SelectedCourt, Sport, VenueId, VenueRegistry, courts_with_stored_names,
};
use crate::ports::SubscriptionRepository;

use super::Clock;

const RETRY_INTERVAL: Duration = Duration::from_secs(60);

pub struct CourtNameConversion {
    store: Arc<dyn SubscriptionRepository>,
    registry: Arc<RwLock<VenueRegistry>>,
    clock: Arc<dyn Clock>,
    warned: HashMap<i64, Vec<String>>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ConversionPass {
    pub converted: usize,
    pub awaiting_catalogs: usize,
    pub unresolvable: usize,
}

enum Plan {
    Convert(Vec<SelectedCourt>),
    AwaitCatalogs,
    Unresolvable(Vec<String>),
}

impl CourtNameConversion {
    pub fn new(
        store: Arc<dyn SubscriptionRepository>,
        registry: Arc<RwLock<VenueRegistry>>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            registry,
            clock,
            warned: HashMap::new(),
        }
    }

    /// Nothing creates a pending row any more, so once none is left this is done
    /// for good. A row that cannot convert yet keeps it retrying, because a later
    /// catalog can resolve it.
    pub async fn run_until_done(mut self) {
        let mut retry = tokio::time::interval(RETRY_INTERVAL);
        loop {
            retry.tick().await;
            match self.run_pass().await {
                Ok(pass) if pass.awaiting_catalogs == 0 && pass.unresolvable == 0 => return,
                Ok(_) => {}
                Err(error) => warn!(
                    error = %format!("{error:#}"),
                    "converting stored court names failed; retrying"
                ),
            }
        }
    }

    pub async fn run_pass(&mut self) -> Result<ConversionPass> {
        let subscriptions = self.store.list_all(self.clock.today()).await?;
        let plans: Vec<(i64, Plan)> = {
            let registry = self.registry.read().expect("venue registry poisoned");
            subscriptions
                .iter()
                .filter_map(|sub| match &sub.courts {
                    CourtSelection::PendingNames(names) => Some((
                        sub.id,
                        plan(&registry, sub.sport, sub.venue.as_ref(), names),
                    )),
                    CourtSelection::Every | CourtSelection::Selected(_) => None,
                })
                .collect()
        };

        let mut pass = ConversionPass::default();
        for (id, plan) in plans {
            match plan {
                Plan::Convert(courts) => {
                    let selected = courts.len();
                    if self.store.convert_pending_courts(id, courts).await? {
                        info!(subscription = id, selected, "converted stored court names");
                        pass.converted += 1;
                    }
                    self.warned.remove(&id);
                }
                Plan::AwaitCatalogs => pass.awaiting_catalogs += 1,
                Plan::Unresolvable(names) => {
                    if self.warned.get(&id) != Some(&names) {
                        warn!(
                            subscription = id,
                            unresolved = ?names,
                            "stored court names match no court, so the subscription stays \
                             unconverted and keeps matching by name"
                        );
                        self.warned.insert(id, names);
                    }
                    pass.unresolvable += 1;
                }
            }
        }
        Ok(pass)
    }
}

fn plan(registry: &VenueRegistry, sport: Sport, venue: Option<&VenueId>, names: &[String]) -> Plan {
    let Some(catalogs) = registry.ready_catalogs(sport, venue) else {
        return Plan::AwaitCatalogs;
    };
    match courts_with_stored_names(&catalogs, names) {
        Ok(courts) => Plan::Convert(courts),
        Err(unresolved) => Plan::Unresolvable(unresolved),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::{NaiveDate, Weekday};
    use tracing::{Event, Level, Subscriber};
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::{Context, SubscriberExt as _};
    use uuid::Uuid;

    use super::*;
    use crate::model::{
        Court, CourtCatalog, CourtFilter, Schedule, TimeRange, UncheckedDraft, Venue,
    };
    use crate::store::SqliteStore;
    use crate::subscriptions::SubscriptionService;
    use crate::subscriptions::contract::SelectedCourtSummary;
    use crate::subscriptions::testing::{
        RecordingSender, clay_court, delivery_route, dispatcher_on, padel_club, padel_court,
        registry_of, service_on, store, tennis_club, tuesday_slot, uref,
    };
    use crate::subscriptions::{AlertDispatcher, BerlinClock};

    fn north() -> VenueId {
        VenueId::new("north")
    }

    fn south() -> VenueId {
        VenueId::new("south")
    }

    struct Deployment {
        store: Arc<SqliteStore>,
        registry: Arc<RwLock<VenueRegistry>>,
        conversion: CourtNameConversion,
        service: Arc<SubscriptionService>,
        dispatcher: Arc<AlertDispatcher>,
        sender: Arc<RecordingSender>,
    }

    impl Deployment {
        async fn of(clubs: Vec<(Venue, Option<Vec<Court>>)>) -> Self {
            Self::over(store().await, clubs)
        }

        /// A process starting over an existing database, with nothing carried over.
        fn over(store: Arc<SqliteStore>, clubs: Vec<(Venue, Option<Vec<Court>>)>) -> Self {
            let registry = registry_of(clubs);
            let service = service_on(store.clone(), registry.clone());
            let sender = RecordingSender::new(false);
            let dispatcher = dispatcher_on(
                store.clone(),
                registry.clone(),
                [delivery_route("discord", sender.clone())],
            );
            Self {
                conversion: CourtNameConversion::new(
                    store.clone(),
                    registry.clone(),
                    Arc::new(BerlinClock),
                ),
                store,
                registry,
                service,
                dispatcher,
                sender,
            }
        }

        async fn stored(
            &self,
            sport: Sport,
            venue: Option<VenueId>,
            courts: CourtSelection,
        ) -> i64 {
            self.store
                .add(
                    UncheckedDraft {
                        subscriber: uref("1"),
                        sport,
                        venue,
                        schedule: Schedule::Weekday(Weekday::Tue),
                        time_range: TimeRange::new(0, 24 * 60).unwrap(),
                        courts,
                        filter: CourtFilter::Any,
                    }
                    .assume_valid(),
                )
                .await
                .unwrap()
        }

        async fn named(&self, sport: Sport, venue: Option<VenueId>, names: &[&str]) -> i64 {
            let names = names.iter().map(|name| (*name).to_owned()).collect();
            self.stored(sport, venue, CourtSelection::PendingNames(names))
                .await
        }

        async fn courts_of(&self, id: i64) -> CourtSelection {
            let long_ago = NaiveDate::from_ymd_opt(2000, 1, 1).unwrap();
            let all = self.store.list_all(long_ago).await.unwrap();
            all.into_iter()
                .find(|sub| sub.id == id)
                .expect("the row is still there")
                .courts
        }

        async fn pass(&mut self) -> ConversionPass {
            self.conversion.run_pass().await.unwrap()
        }

        fn set_courts(&self, venue: &VenueId, courts: Vec<Court>) {
            self.registry
                .write()
                .unwrap()
                .set_catalog(venue, CourtCatalog::new(courts));
        }

        /// Dispatches a matching slot and returns how many alerts that added.
        async fn alerts_for(&self, venue: &VenueId, court: u128, name: &str) -> usize {
            let before = self.sender.sent.lock().unwrap().len();
            self.dispatcher
                .dispatch(&[tuesday_slot(venue, court, name)])
                .await;
            self.sender.sent.lock().unwrap().len() - before
        }
    }

    fn selected(courts: &[(VenueId, u128)]) -> CourtSelection {
        CourtSelection::Selected(
            courts
                .iter()
                .map(|(venue_id, court)| SelectedCourt {
                    venue_id: venue_id.clone(),
                    court_id: Uuid::from_u128(*court),
                })
                .collect(),
        )
    }

    fn pending(names: &[&str]) -> CourtSelection {
        CourtSelection::PendingNames(names.iter().map(|name| (*name).to_owned()).collect())
    }

    fn converted(count: usize) -> ConversionPass {
        ConversionPass {
            converted: count,
            ..ConversionPass::default()
        }
    }

    fn two_ready_tennis_clubs() -> Vec<(Venue, Option<Vec<Court>>)> {
        vec![
            (
                tennis_club("north"),
                Some(vec![clay_court(1, "Court 2"), clay_court(2, "Court 3")]),
            ),
            (tennis_club("south"), Some(vec![clay_court(3, "Court 2")])),
        ]
    }

    #[tokio::test]
    async fn a_row_with_a_club_converts_to_the_courts_of_that_club_only() {
        let mut deployment = Deployment::of(two_ready_tennis_clubs()).await;
        let id = deployment
            .named(Sport::Tennis, Some(south()), &["Court 2"])
            .await;

        assert_eq!(deployment.pass().await, converted(1));

        assert_eq!(deployment.courts_of(id).await, selected(&[(south(), 3)]));
    }

    #[tokio::test]
    async fn a_row_with_no_club_converts_to_every_club_of_its_sport() {
        let mut clubs = two_ready_tennis_clubs();
        clubs.push((padel_club("padel"), Some(vec![padel_court(9, "Court 2")])));
        let mut deployment = Deployment::of(clubs).await;
        let id = deployment.named(Sport::Tennis, None, &["Court 2"]).await;

        assert_eq!(deployment.pass().await, converted(1));

        assert_eq!(
            deployment.courts_of(id).await,
            selected(&[(north(), 1), (south(), 3)])
        );
    }

    #[tokio::test]
    async fn a_name_matching_several_courts_keeps_all_of_them() {
        let mut deployment = Deployment::of(vec![(
            tennis_club("north"),
            Some(vec![
                clay_court(1, "Centre Court"),
                clay_court(2, "Centre Court"),
            ]),
        )])
        .await;
        let id = deployment
            .named(Sport::Tennis, Some(north()), &["Centre Court"])
            .await;

        deployment.pass().await;

        assert_eq!(
            deployment.courts_of(id).await,
            selected(&[(north(), 1), (north(), 2)])
        );
    }

    #[tokio::test]
    async fn stored_names_compare_case_insensitively_as_the_matcher_does() {
        let mut deployment = Deployment::of(vec![(
            tennis_club("north"),
            Some(vec![clay_court(1, "Court 2"), clay_court(2, "Platz Süd")]),
        )])
        .await;
        let id = deployment
            .named(Sport::Tennis, None, &["COURT 2", "platz SÜD"])
            .await;

        assert_eq!(deployment.pass().await, converted(1));

        assert_eq!(
            deployment.courts_of(id).await,
            selected(&[(north(), 1), (north(), 2)])
        );
    }

    #[tokio::test]
    async fn names_of_one_court_in_two_spellings_select_it_once() {
        let mut deployment = Deployment::of(two_ready_tennis_clubs()).await;
        let id = deployment
            .named(Sport::Tennis, Some(north()), &["Court 2", "court 2"])
            .await;

        deployment.pass().await;

        assert_eq!(deployment.courts_of(id).await, selected(&[(north(), 1)]));
    }

    #[tokio::test]
    async fn a_row_beyond_the_input_limits_still_converts() {
        let courts = (1..=25).map(|id| clay_court(id, &format!("Court {id}")));
        let mut deployment =
            Deployment::of(vec![(tennis_club("north"), Some(courts.collect()))]).await;
        let names: Vec<String> = (1..=25).map(|id| format!("Court {id}")).collect();
        let id = deployment
            .stored(Sport::Tennis, None, CourtSelection::PendingNames(names))
            .await;

        assert_eq!(deployment.pass().await, converted(1));

        let all: Vec<(VenueId, u128)> = (1..=25).map(|court| (north(), court)).collect();
        assert_eq!(deployment.courts_of(id).await, selected(&all));
    }

    #[tokio::test]
    async fn number_shorthand_does_not_apply_to_stored_names() {
        let mut deployment = Deployment::of(two_ready_tennis_clubs()).await;
        let id = deployment.named(Sport::Tennis, None, &["2"]).await;

        let pass = deployment.pass().await;

        assert_eq!(pass.unresolvable, 1);
        assert_eq!(deployment.courts_of(id).await, pending(&["2"]));
    }

    #[tokio::test]
    async fn a_row_with_no_club_waits_for_every_catalog_of_its_sport() {
        let mut deployment = Deployment::of(vec![
            (tennis_club("north"), Some(vec![clay_court(1, "Court 2")])),
            (tennis_club("south"), None),
        ])
        .await;
        let id = deployment.named(Sport::Tennis, None, &["Court 2"]).await;

        let pass = deployment.pass().await;

        assert_eq!(pass.awaiting_catalogs, 1);
        assert_eq!(pass.converted, 0);
        assert_eq!(deployment.courts_of(id).await, pending(&["Court 2"]));
        assert_eq!(
            deployment.alerts_for(&north(), 1, "Court 2").await,
            1,
            "a waiting row keeps matching by name"
        );

        deployment.set_courts(&south(), vec![clay_court(3, "Court 2")]);

        assert_eq!(deployment.pass().await, converted(1));
        assert_eq!(
            deployment.courts_of(id).await,
            selected(&[(north(), 1), (south(), 3)])
        );
        assert_eq!(
            deployment.alerts_for(&south(), 3, "Court 2").await,
            1,
            "the court of the club it waited for is selected"
        );
    }

    #[tokio::test]
    async fn a_row_with_a_club_waits_for_that_club_alone() {
        let mut deployment = Deployment::of(vec![
            (tennis_club("north"), Some(vec![clay_court(1, "Court 2")])),
            (tennis_club("south"), None),
        ])
        .await;
        let ready = deployment
            .named(Sport::Tennis, Some(north()), &["Court 2"])
            .await;
        let waiting = deployment
            .named(Sport::Tennis, Some(south()), &["Court 2"])
            .await;

        let pass = deployment.pass().await;

        assert_eq!(pass.converted, 1);
        assert_eq!(pass.awaiting_catalogs, 1);
        assert_eq!(deployment.courts_of(ready).await, selected(&[(north(), 1)]));
        assert_eq!(deployment.courts_of(waiting).await, pending(&["Court 2"]));
    }

    #[tokio::test]
    async fn a_row_with_any_unresolvable_name_stays_unconverted_as_a_whole() {
        let mut deployment = Deployment::of(two_ready_tennis_clubs()).await;
        let id = deployment
            .named(Sport::Tennis, None, &["Court 2", "Retired Court"])
            .await;

        let pass = deployment.pass().await;

        assert_eq!(pass.unresolvable, 1);
        assert_eq!(pass.converted, 0);
        assert_eq!(
            deployment.courts_of(id).await,
            pending(&["Court 2", "Retired Court"])
        );
        assert_eq!(
            deployment.alerts_for(&north(), 1, "Court 2").await,
            1,
            "the part of the selection that still resolves keeps alerting"
        );
    }

    #[derive(Clone, Default)]
    struct WarningCount(Arc<AtomicUsize>);

    impl<S: Subscriber> Layer<S> for WarningCount {
        fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
            if event.metadata().level() == &Level::WARN {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    #[tokio::test]
    async fn an_unresolvable_row_warns_once_until_what_it_cannot_resolve_changes() {
        let warnings = WarningCount::default();
        let _logging =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(warnings.clone()));
        let mut deployment = Deployment::of(two_ready_tennis_clubs()).await;
        deployment
            .named(Sport::Tennis, None, &["Lost One", "Lost Two"])
            .await;

        deployment.pass().await;
        deployment.pass().await;
        assert_eq!(warnings.0.load(Ordering::SeqCst), 1);

        deployment.set_courts(&north(), vec![clay_court(1, "Lost One")]);
        deployment.pass().await;
        deployment.pass().await;
        assert_eq!(
            warnings.0.load(Ordering::SeqCst),
            2,
            "one name now resolves, so what is left deserves its own warning"
        );
    }

    #[tokio::test]
    async fn a_row_converts_once_a_refreshed_catalog_can_resolve_it() {
        let mut deployment = Deployment::of(two_ready_tennis_clubs()).await;
        let id = deployment
            .named(Sport::Tennis, Some(north()), &["Court 9"])
            .await;
        assert_eq!(deployment.pass().await.unresolvable, 1);

        deployment.set_courts(&north(), vec![clay_court(9, "Court 9")]);

        assert_eq!(deployment.pass().await, converted(1));
        assert_eq!(deployment.courts_of(id).await, selected(&[(north(), 9)]));
    }

    #[tokio::test]
    async fn a_club_no_longer_configured_leaves_its_row_unconverted() {
        let mut deployment = Deployment::of(two_ready_tennis_clubs()).await;
        let id = deployment
            .named(Sport::Tennis, Some(VenueId::new("closed")), &["Court 2"])
            .await;
        let no_clubs = deployment.named(Sport::Padel, None, &["Court 2"]).await;

        let pass = deployment.pass().await;

        assert_eq!(pass.unresolvable, 2);
        assert_eq!(deployment.courts_of(id).await, pending(&["Court 2"]));
        assert_eq!(deployment.courts_of(no_clubs).await, pending(&["Court 2"]));
    }

    #[tokio::test]
    async fn a_converted_row_is_not_derived_from_names_again() {
        let mut deployment = Deployment::of(vec![(
            tennis_club("north"),
            Some(vec![clay_court(1, "Court 2")]),
        )])
        .await;
        let id = deployment.named(Sport::Tennis, None, &["Court 2"]).await;
        deployment.pass().await;

        deployment.set_courts(
            &north(),
            vec![clay_court(1, "Show Court"), clay_court(7, "Court 2")],
        );

        assert_eq!(deployment.pass().await, ConversionPass::default());
        assert_eq!(deployment.courts_of(id).await, selected(&[(north(), 1)]));
        assert_eq!(
            deployment.alerts_for(&north(), 7, "Court 2").await,
            0,
            "a court taking the old name is not picked up"
        );
        assert_eq!(
            deployment.alerts_for(&north(), 1, "Show Court").await,
            1,
            "the converted court keeps alerting under its new name"
        );
    }

    #[tokio::test]
    async fn conversion_carries_across_restarts() {
        let clubs = || {
            vec![
                (tennis_club("north"), Some(vec![clay_court(1, "Court 2")])),
                (tennis_club("south"), None),
            ]
        };
        let mut first = Deployment::of(clubs()).await;
        let store = first.store.clone();
        let done = first
            .named(Sport::Tennis, Some(north()), &["Court 2"])
            .await;
        let waiting = first.named(Sport::Tennis, None, &["Court 2"]).await;
        first.pass().await;
        drop(first);

        let mut second = Deployment::over(store.clone(), clubs());
        assert_eq!(
            second.pass().await.converted,
            0,
            "nothing new could convert"
        );
        assert_eq!(second.courts_of(done).await, selected(&[(north(), 1)]));
        assert_eq!(second.courts_of(waiting).await, pending(&["Court 2"]));
        second.set_courts(&south(), vec![clay_court(3, "Court 2")]);
        assert_eq!(second.pass().await, converted(1));
        drop(second);

        let mut third = Deployment::over(
            store,
            vec![
                (tennis_club("north"), Some(vec![clay_court(1, "Renamed")])),
                (tennis_club("south"), None),
            ],
        );
        assert_eq!(third.pass().await, ConversionPass::default());
        assert_eq!(
            third.courts_of(waiting).await,
            selected(&[(north(), 1), (south(), 3)])
        );
    }

    #[tokio::test]
    async fn rows_with_nothing_to_convert_are_left_alone() {
        let mut deployment = Deployment::of(two_ready_tennis_clubs()).await;
        let every = deployment
            .stored(Sport::Tennis, None, CourtSelection::Every)
            .await;
        let chosen = deployment
            .stored(Sport::Tennis, None, selected(&[(south(), 3)]))
            .await;

        assert_eq!(deployment.pass().await, ConversionPass::default());

        assert_eq!(deployment.courts_of(every).await, CourtSelection::Every);
        assert_eq!(
            deployment.courts_of(chosen).await,
            selected(&[(south(), 3)])
        );
    }

    #[tokio::test]
    async fn a_stored_empty_list_still_matches_nothing_after_converting() {
        let mut deployment = Deployment::of(two_ready_tennis_clubs()).await;
        let id = deployment.named(Sport::Tennis, None, &[]).await;
        assert_eq!(deployment.alerts_for(&north(), 1, "Court 2").await, 0);

        assert_eq!(deployment.pass().await, converted(1));

        assert_eq!(deployment.courts_of(id).await, selected(&[]));
        assert_eq!(deployment.alerts_for(&north(), 1, "Court 2").await, 0);
    }

    #[tokio::test]
    async fn an_expired_row_is_not_worth_converting() {
        let mut deployment = Deployment::of(two_ready_tennis_clubs()).await;
        let id = deployment
            .store
            .add(
                UncheckedDraft {
                    subscriber: uref("1"),
                    sport: Sport::Tennis,
                    venue: None,
                    schedule: Schedule::Date(NaiveDate::from_ymd_opt(2000, 1, 1).unwrap()),
                    time_range: TimeRange::new(0, 24 * 60).unwrap(),
                    courts: pending(&["Court 2"]),
                    filter: CourtFilter::Any,
                }
                .assume_valid(),
            )
            .await
            .unwrap();

        assert_eq!(deployment.pass().await, ConversionPass::default());
        assert_eq!(deployment.courts_of(id).await, pending(&["Court 2"]));
    }

    #[tokio::test]
    async fn a_converted_row_lists_its_ids_with_current_names() {
        let mut deployment = Deployment::of(two_ready_tennis_clubs()).await;
        deployment
            .named(Sport::Tennis, Some(south()), &["court 2"])
            .await;
        deployment.pass().await;

        let subs = deployment.service.list(&uref("1")).await.unwrap();

        assert_eq!(
            subs[0].courts,
            Some(vec![SelectedCourtSummary::Identified {
                id: Uuid::from_u128(3),
                name: Some("Court 2".into()),
            }])
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_loop_retries_until_every_row_has_converted_and_then_stops() {
        let deployment = Deployment::of(vec![
            (tennis_club("north"), Some(vec![clay_court(1, "Court 2")])),
            (tennis_club("south"), None),
        ])
        .await;
        let id = deployment.named(Sport::Tennis, None, &["Court 2"]).await;
        let conversion = CourtNameConversion::new(
            deployment.store.clone(),
            deployment.registry.clone(),
            Arc::new(BerlinClock),
        );

        let running = tokio::spawn(conversion.run_until_done());
        tokio::time::sleep(RETRY_INTERVAL * 3).await;
        assert!(
            !running.is_finished(),
            "a row is still waiting on a catalog"
        );
        assert_eq!(deployment.courts_of(id).await, pending(&["Court 2"]));

        deployment.set_courts(&south(), vec![clay_court(3, "Court 2")]);

        tokio::time::timeout(RETRY_INTERVAL * 2, running)
            .await
            .expect("nothing is pending, so the loop should have stopped")
            .unwrap();
        assert_eq!(
            deployment.courts_of(id).await,
            selected(&[(north(), 1), (south(), 3)])
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_loop_stops_at_once_when_nothing_is_pending() {
        let deployment = Deployment::of(two_ready_tennis_clubs()).await;
        deployment
            .stored(Sport::Tennis, None, CourtSelection::Every)
            .await;

        tokio::time::timeout(
            Duration::from_secs(1),
            deployment.conversion.run_until_done(),
        )
        .await
        .expect("the first pass found nothing to do");
    }
}
