use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use futures::stream::{self, StreamExt};
use tokio::sync::mpsc::Receiver;
use tracing::warn;

use crate::model::{AvailabilityChange, IntegrationId, VenueRegistry};
use crate::ports::SubscriptionRepository;
use crate::subscriptions::contract::{AlertMessageUpdater, AlertSender, AvailabilityAlert};

use super::matcher::match_subscriptions;
use super::{Clock, slot_summary};

const MAX_CONCURRENT_ALERTS: usize = 16;

struct AlertRoute {
    sender: Arc<dyn AlertSender>,
    updater: Option<Arc<dyn AlertMessageUpdater>>,
}

pub struct AlertDispatcher {
    store: Arc<dyn SubscriptionRepository>,
    registry: Arc<RwLock<VenueRegistry>>,
    routes: RwLock<HashMap<IntegrationId, AlertRoute>>,
    clock: Arc<dyn Clock>,
}

impl AlertDispatcher {
    pub fn new(
        store: Arc<dyn SubscriptionRepository>,
        registry: Arc<RwLock<VenueRegistry>>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            registry,
            routes: RwLock::new(HashMap::new()),
            clock,
        }
    }

    pub fn register_sender(&self, integration: IntegrationId, sender: Arc<dyn AlertSender>) {
        self.add_route(
            integration,
            AlertRoute {
                sender,
                updater: None,
            },
        );
    }

    pub fn register_chat_provider<P>(&self, integration: IntegrationId, provider: Arc<P>)
    where
        P: AlertSender + AlertMessageUpdater + 'static,
    {
        self.add_route(
            integration,
            AlertRoute {
                sender: provider.clone(),
                updater: Some(provider),
            },
        );
    }

    fn add_route(&self, integration: IntegrationId, route: AlertRoute) {
        self.routes
            .write()
            .expect("routes lock poisoned")
            .insert(integration, route);
    }

    pub async fn dispatch_loop(self: Arc<Self>, mut rx: Receiver<Vec<AvailabilityChange>>) {
        let mut last_expiry_cleanup = None;
        while let Some(changes) = rx.recv().await {
            self.cleanup_expired_if_needed(&mut last_expiry_cleanup)
                .await;
            self.dispatch(&changes).await;
        }
    }

    async fn cleanup_expired_if_needed(&self, last_cleanup: &mut Option<chrono::NaiveDate>) {
        let today = self.clock.today();
        if *last_cleanup == Some(today) {
            return;
        }
        match self.store.remove_expired(today).await {
            Ok(_) => *last_cleanup = Some(today),
            Err(e) => {
                warn!(error = %format!("{e:#}"), "dispatch: failed to expire old date subscriptions");
            }
        }
    }

    pub async fn dispatch(&self, changes: &[AvailabilityChange]) {
        if changes.is_empty() {
            return;
        }
        self.alert_subscribers(changes).await;
        self.mark_unbookable(changes).await;
    }

    async fn alert_subscribers(&self, changes: &[AvailabilityChange]) {
        let subs = match self.store.list_all(self.clock.today()).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %format!("{e:#}"), "dispatch: failed to load subscriptions");
                return;
            }
        };
        if subs.is_empty() {
            return;
        }
        let matched = {
            let registry = self.registry.read().expect("venue registry poisoned");
            match_subscriptions(changes, &subs, &registry)
        };
        let deliveries: Vec<_> = {
            let registry = self.registry.read().expect("venue registry poisoned");
            let routes = self.routes.read().expect("routes lock poisoned");
            matched
                .into_iter()
                .filter_map(|(subscriber, slots)| {
                    let Some(route) = routes.get(&subscriber.integration) else {
                        warn!(
                            integration = %subscriber.integration,
                            subscriber = %subscriber.subscriber_id,
                            "dispatch: no sender registered for integration"
                        );
                        return None;
                    };
                    let alert = AvailabilityAlert {
                        slots: slots
                            .iter()
                            .map(|slot| slot_summary(&registry, slot))
                            .collect(),
                    };
                    Some((subscriber, route.sender.clone(), alert))
                })
                .collect()
        };
        stream::iter(deliveries)
            .for_each_concurrent(
                MAX_CONCURRENT_ALERTS,
                |(subscriber, sender, alert)| async move {
                    if let Err(e) = sender.send_alert(&subscriber.subscriber_id, &alert).await {
                        warn!(
                            error = %format!("{e:#}"),
                            integration = %subscriber.integration,
                            subscriber = %subscriber.subscriber_id,
                            "dispatch: failed to send alert"
                        );
                    }
                },
            )
            .await;
    }

    async fn mark_unbookable(&self, changes: &[AvailabilityChange]) {
        let taken = AvailabilityChange::taken_ids(changes);
        if taken.is_empty() {
            return;
        }
        let updaters: Vec<(IntegrationId, Arc<dyn AlertMessageUpdater>)> = self
            .routes
            .read()
            .expect("routes lock poisoned")
            .iter()
            .filter_map(|(integration, route)| {
                let updater = route.updater.clone()?;
                Some((integration.clone(), updater))
            })
            .collect();
        for (integration, updater) in updaters {
            if let Err(e) = updater.mark_unbookable(&taken).await {
                warn!(
                    error = %format!("{e:#}"),
                    integration = %integration,
                    slots = taken.len(),
                    "dispatch: failed to mark slots that stopped being bookable"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{
        INDOOR_COURT, PADEL_CLUB_NAME, RecordingSender, court_id, dispatcher_on, padel_club,
        padel_court, padel_court_id, padel_venue_id, registry, registry_of, service_and_dispatcher,
        service_and_dispatcher_over, store, subscribe_request, subscribe_to_courts, tuesday_slot,
        uref, venue_id,
    };
    use crate::model::{
        AvailabilityChange, BookableSlot, BookableSlotId, CourtCatalog, CourtFilter,
        CourtSelection, Schedule, Sport, TimeRange, UncheckedDraft, VenueId, VenueRegistry,
    };
    use crate::ports::SubscriptionRepository;
    use crate::subscriptions::AlertDispatcher;
    use crate::subscriptions::contract::{
        AlertSender, AvailabilityAlert, CourtChoice, SubscriptionRequest,
    };
    use crate::time::today_berlin;
    use chrono::{TimeZone, Utc};
    use std::sync::{Arc, RwLock};
    use uuid::Uuid;

    fn unbookable(name: &str) -> AvailabilityChange {
        match bookable(name) {
            AvailabilityChange::BecameBookable(slot) => AvailabilityChange::BecameUnbookable(slot),
            change => change,
        }
    }

    fn bookable(name: &str) -> AvailabilityChange {
        let starts_at = Utc.with_ymd_and_hms(2026, 6, 2, 18, 0, 0).unwrap();
        AvailabilityChange::BecameBookable(BookableSlot {
            venue_id: venue_id(),
            court_id: court_id(name),
            court_name: name.into(),
            starts_at,
            ends_at: starts_at + chrono::Duration::hours(1),
            available_places: 1,
        })
    }

    fn padel_bookable() -> AvailabilityChange {
        let starts_at = Utc.with_ymd_and_hms(2026, 6, 2, 18, 0, 0).unwrap();
        AvailabilityChange::BecameBookable(BookableSlot {
            venue_id: padel_venue_id(),
            court_id: padel_court_id(INDOOR_COURT),
            court_name: INDOOR_COURT.into(),
            starts_at,
            ends_at: starts_at + chrono::Duration::hours(1),
            available_places: 1,
        })
    }

    struct RendezvousSender(tokio::sync::Barrier);

    #[async_trait::async_trait]
    impl AlertSender for RendezvousSender {
        async fn send_alert(&self, _: &str, _: &AvailabilityAlert) -> anyhow::Result<()> {
            self.0.wait().await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_slow_alert_does_not_hold_back_the_next_subscribers() {
        let (svc, dispatcher) = service_and_dispatcher().await;
        dispatcher.register_sender(
            "discord".into(),
            Arc::new(RendezvousSender(tokio::sync::Barrier::new(2))),
        );
        for subscriber in ["1", "2"] {
            svc.subscribe(&uref(subscriber), &subscribe_request(18 * 60, 22 * 60))
                .await
                .unwrap()
                .unwrap();
        }

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            dispatcher.dispatch(&[bookable("Court 2")]),
        )
        .await
        .expect("each alert waits for the other, so they must be in flight together");
    }

    #[tokio::test]
    async fn dispatch_sends_alert_to_matching_subscriber() {
        let (svc, dispatcher) = service_and_dispatcher().await;
        let sender = RecordingSender::new(false);
        dispatcher.register_sender("discord".into(), sender.clone());
        svc.subscribe(&uref("1"), &subscribe_request(18 * 60, 22 * 60))
            .await
            .unwrap()
            .unwrap();

        dispatcher.dispatch(&[bookable("Court 2")]).await;

        let sent = sender.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "1");
        assert_eq!(sent[0].1.slots.len(), 1);
        assert_eq!(sent[0].1.slots[0].court, "Court 2");
    }

    #[tokio::test]
    async fn an_alert_names_the_club_each_court_belongs_to() {
        let (svc, dispatcher) = service_and_dispatcher().await;
        let sender = RecordingSender::new(false);
        dispatcher.register_sender("discord".into(), sender.clone());
        svc.subscribe(&uref("1"), &subscribe_request(18 * 60, 22 * 60))
            .await
            .unwrap()
            .unwrap();

        dispatcher.dispatch(&[bookable("Court 2")]).await;

        let sent = sender.sent.lock().unwrap();
        assert_eq!(sent[0].1.slots[0].club, "ZHS München");
    }

    #[tokio::test]
    async fn courts_of_the_same_name_at_different_clubs_are_distinguishable() {
        let (svc, dispatcher) = service_and_dispatcher().await;
        let sender = RecordingSender::new(false);
        dispatcher.register_sender("discord".into(), sender.clone());
        svc.subscribe(
            &uref("1"),
            &SubscriptionRequest {
                sport: Sport::Padel,
                venue: None,
                schedule: Schedule::Weekday(chrono::Weekday::Tue),
                start_minute: 18 * 60,
                end_minute: 22 * 60,
                courts: None,
                filter: None,
            },
        )
        .await
        .unwrap()
        .unwrap();

        dispatcher.dispatch(&[padel_bookable()]).await;

        let sent = sender.sent.lock().unwrap();
        assert_eq!(sent[0].1.slots.len(), 1);
        assert_eq!(sent[0].1.slots[0].club, PADEL_CLUB_NAME);
        assert_ne!(
            sent[0].1.slots[0].club, "ZHS München",
            "the padel court was labelled with the tennis club"
        );
    }

    #[tokio::test]
    async fn dispatch_skips_non_matching_subscriber() {
        let (svc, dispatcher) = service_and_dispatcher().await;
        let sender = RecordingSender::new(false);
        dispatcher.register_sender("discord".into(), sender.clone());
        svc.subscribe(&uref("1"), &subscribe_request(8 * 60, 10 * 60))
            .await
            .unwrap()
            .unwrap();

        dispatcher.dispatch(&[bookable("Court 2")]).await;

        assert!(sender.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dispatch_failing_send_does_not_abort_batch() {
        let (svc, dispatcher) = service_and_dispatcher().await;
        let sender = RecordingSender::new(true); // every send errors
        dispatcher.register_sender("discord".into(), sender.clone());
        svc.subscribe(&uref("1"), &subscribe_request(18 * 60, 22 * 60))
            .await
            .unwrap()
            .unwrap();
        svc.subscribe(&uref("2"), &subscribe_request(18 * 60, 22 * 60))
            .await
            .unwrap()
            .unwrap();

        dispatcher.dispatch(&[bookable("Court 2")]).await;

        assert_eq!(sender.sent.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn dispatch_without_registered_sender_does_not_panic() {
        let (svc, dispatcher) = service_and_dispatcher().await;
        svc.subscribe(&uref("1"), &subscribe_request(18 * 60, 22 * 60))
            .await
            .unwrap()
            .unwrap();
        dispatcher.dispatch(&[bookable("Court 2")]).await;
    }

    #[tokio::test]
    async fn a_slot_that_stops_being_bookable_is_struck_where_it_was_announced() {
        let (_, dispatcher) = service_and_dispatcher().await;
        let sender = RecordingSender::new(false);
        dispatcher.register_chat_provider("discord".into(), sender.clone());
        let taken = unbookable("Court 2");

        dispatcher.dispatch(std::slice::from_ref(&taken)).await;

        assert_eq!(sender.struck(), vec![BookableSlotId::from(taken.slot())]);
        assert!(
            sender.sent.lock().unwrap().is_empty(),
            "a court going away is not news worth a fresh alert"
        );
    }

    #[tokio::test]
    async fn slots_are_struck_even_when_no_subscription_matches_them() {
        let (_, dispatcher) = service_and_dispatcher().await;
        let sender = RecordingSender::new(false);
        dispatcher.register_chat_provider("discord".into(), sender.clone());
        let taken = unbookable("Court 2");

        dispatcher.dispatch(std::slice::from_ref(&taken)).await;

        assert_eq!(sender.struck(), vec![BookableSlotId::from(taken.slot())]);
    }

    #[tokio::test]
    async fn alerts_go_out_before_anything_is_struck() {
        let (svc, dispatcher) = service_and_dispatcher().await;
        let sender = RecordingSender::new(false);
        dispatcher.register_chat_provider("discord".into(), sender.clone());
        svc.subscribe(&uref("1"), &subscribe_request(18 * 60, 22 * 60))
            .await
            .unwrap()
            .unwrap();

        dispatcher
            .dispatch(&[unbookable("Court 5"), bookable("Court 2")])
            .await;

        assert_eq!(sender.order(), vec!["send", "strike"]);
    }

    #[tokio::test]
    async fn a_batch_of_new_slots_strikes_nothing() {
        let (svc, dispatcher) = service_and_dispatcher().await;
        let sender = RecordingSender::new(false);
        dispatcher.register_chat_provider("discord".into(), sender.clone());
        svc.subscribe(&uref("1"), &subscribe_request(18 * 60, 22 * 60))
            .await
            .unwrap()
            .unwrap();

        dispatcher.dispatch(&[bookable("Court 2")]).await;

        assert!(sender.struck().is_empty());
        assert_eq!(sender.sent.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_failing_strike_does_not_stop_the_alerts_in_the_same_batch() {
        let (svc, dispatcher) = service_and_dispatcher().await;
        let sender = RecordingSender::new(true);
        dispatcher.register_chat_provider("discord".into(), sender.clone());
        svc.subscribe(&uref("1"), &subscribe_request(18 * 60, 22 * 60))
            .await
            .unwrap()
            .unwrap();

        dispatcher
            .dispatch(&[unbookable("Court 5"), bookable("Court 2")])
            .await;

        assert_eq!(sender.struck().len(), 1);
        assert_eq!(sender.sent.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_failing_update_does_not_stop_other_integrations() {
        let (_, dispatcher) = service_and_dispatcher().await;
        let first = RecordingSender::new(true);
        let second = RecordingSender::new(true);
        dispatcher.register_chat_provider("discord".into(), first.clone());
        dispatcher.register_chat_provider("slack".into(), second.clone());

        dispatcher.dispatch(&[unbookable("Court 2")]).await;

        assert_eq!(first.struck().len(), 1);
        assert_eq!(second.struck().len(), 1);
    }

    #[tokio::test]
    async fn a_delivery_only_route_is_alerted_but_never_asked_to_update() {
        let (svc, dispatcher) = service_and_dispatcher().await;
        let sender = RecordingSender::new(false);
        dispatcher.register_sender("discord".into(), sender.clone());
        svc.subscribe(&uref("1"), &subscribe_request(18 * 60, 22 * 60))
            .await
            .unwrap()
            .unwrap();

        dispatcher
            .dispatch(&[unbookable("Court 5"), bookable("Court 2")])
            .await;

        assert_eq!(sender.order(), vec!["send"]);
        assert!(sender.struck().is_empty());
    }

    #[tokio::test]
    async fn an_update_capable_route_is_updated_alongside_a_delivery_only_route() {
        let (_, dispatcher) = service_and_dispatcher().await;
        let chat = RecordingSender::new(false);
        let delivery_only = RecordingSender::new(false);
        dispatcher.register_chat_provider("discord".into(), chat.clone());
        dispatcher.register_sender("webhook".into(), delivery_only.clone());
        let taken = unbookable("Court 2");

        dispatcher.dispatch(std::slice::from_ref(&taken)).await;

        assert_eq!(chat.struck(), vec![BookableSlotId::from(taken.slot())]);
        assert!(delivery_only.struck().is_empty());
    }

    const COURT_A: u128 = 1;
    const COURT_B: u128 = 2;

    /// One club, two courts with the same name, a subscriber who chose only A.
    async fn subscribed_to_court_a() -> (
        Arc<AlertDispatcher>,
        Arc<RecordingSender>,
        Arc<RwLock<VenueRegistry>>,
    ) {
        let club = VenueId::new("club");
        let registry = registry_of(vec![(
            padel_club("club"),
            Some(vec![
                padel_court(COURT_A, "Centre Court"),
                padel_court(COURT_B, "Centre Court"),
            ]),
        )]);
        let (svc, dispatcher) = service_and_dispatcher_over(registry.clone()).await;
        let sender = RecordingSender::new(false);
        dispatcher.register_sender("discord".into(), sender.clone());
        svc.subscribe(
            &uref("1"),
            &subscribe_to_courts(
                Sport::Padel,
                Some(&club),
                CourtChoice::Ids(vec![Uuid::from_u128(COURT_A)]),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        (dispatcher, sender, registry)
    }

    #[tokio::test]
    async fn a_selected_court_keeps_alerting_after_it_is_renamed() {
        let (dispatcher, sender, registry) = subscribed_to_court_a().await;
        let club = VenueId::new("club");
        registry.write().unwrap().set_catalog(
            &club,
            CourtCatalog::new(vec![padel_court(COURT_A, "Show Court")]),
        );

        dispatcher
            .dispatch(&[tuesday_slot(&club, COURT_A, "Show Court")])
            .await;

        let sent = sender.sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "a renamed court stopped alerting");
        assert_eq!(sent[0].1.slots[0].court_id, Uuid::from_u128(COURT_A));
    }

    #[tokio::test]
    async fn a_selected_court_does_not_alert_for_another_court_of_the_same_name() {
        let (dispatcher, sender, _) = subscribed_to_court_a().await;

        dispatcher
            .dispatch(&[tuesday_slot(&VenueId::new("club"), COURT_B, "Centre Court")])
            .await;

        assert!(
            sender.sent.lock().unwrap().is_empty(),
            "court B alerted a subscriber who chose only court A"
        );
    }

    #[tokio::test]
    async fn a_name_does_not_pick_up_a_court_that_takes_it_later() {
        let club = VenueId::new("club");
        let registry = registry_of(vec![(
            padel_club("club"),
            Some(vec![padel_court(COURT_A, "Centre Court")]),
        )]);
        let (svc, dispatcher) = service_and_dispatcher_over(registry.clone()).await;
        let sender = RecordingSender::new(false);
        dispatcher.register_sender("discord".into(), sender.clone());
        svc.subscribe(
            &uref("1"),
            &subscribe_to_courts(
                Sport::Padel,
                None,
                CourtChoice::Names(vec!["Centre Court".into()]),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        registry.write().unwrap().set_catalog(
            &club,
            CourtCatalog::new(vec![
                padel_court(COURT_A, "Centre Court"),
                padel_court(COURT_B, "Centre Court"),
            ]),
        );

        dispatcher
            .dispatch(&[tuesday_slot(&club, COURT_B, "Centre Court")])
            .await;
        assert!(sender.sent.lock().unwrap().is_empty());

        dispatcher
            .dispatch(&[tuesday_slot(&club, COURT_A, "Centre Court")])
            .await;
        assert_eq!(sender.sent.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn daily_cleanup_removes_expired_date_subscriptions() {
        let store = store().await;
        let dispatcher = dispatcher_on(store.clone(), registry());
        store
            .add(
                UncheckedDraft {
                    subscriber: uref("1"),
                    sport: Sport::Tennis,
                    venue: None,
                    schedule: Schedule::Date(chrono::NaiveDate::from_ymd_opt(2000, 1, 1).unwrap()),
                    time_range: TimeRange::new(0, 24 * 60).unwrap(),
                    courts: CourtSelection::Every,
                    filter: CourtFilter::Any,
                }
                .assume_valid(),
            )
            .await
            .unwrap();
        let before_expiry = chrono::NaiveDate::from_ymd_opt(1900, 1, 1).unwrap();
        assert_eq!(store.list_all(before_expiry).await.unwrap().len(), 1);

        let mut last_cleanup = None;
        dispatcher
            .cleanup_expired_if_needed(&mut last_cleanup)
            .await;

        assert_eq!(store.list_all(before_expiry).await.unwrap().len(), 0);
        assert_eq!(last_cleanup, Some(today_berlin()));

        store
            .add(
                UncheckedDraft {
                    subscriber: uref("1"),
                    sport: Sport::Tennis,
                    venue: None,
                    schedule: Schedule::Date(chrono::NaiveDate::from_ymd_opt(2000, 1, 1).unwrap()),
                    time_range: TimeRange::new(0, 24 * 60).unwrap(),
                    courts: CourtSelection::Every,
                    filter: CourtFilter::Any,
                }
                .assume_valid(),
            )
            .await
            .unwrap();
        dispatcher
            .cleanup_expired_if_needed(&mut last_cleanup)
            .await;
        assert_eq!(store.list_all(before_expiry).await.unwrap().len(), 1);
    }
}
