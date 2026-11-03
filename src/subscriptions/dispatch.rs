use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use futures::stream::{self, StreamExt};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::task::{JoinError, JoinSet};
use tracing::warn;

use crate::model::{
    AvailabilityChange, BookableSlotId, IntegrationId, SubscriberRef, VenueRegistry,
};
use crate::ports::SubscriptionRepository;
use crate::subscriptions::contract::{AlertMessageUpdater, AlertSender, AvailabilityAlert};

use super::matcher::match_subscriptions;
use super::{Clock, slot_summary};

const MAX_CONCURRENT_ALERTS: usize = 16;
const MAX_QUEUED_BATCHES: usize = 8;

#[derive(Clone)]
pub struct AlertRoute {
    sender: Arc<dyn AlertSender>,
    updater: Option<Arc<dyn AlertMessageUpdater>>,
}

impl AlertRoute {
    pub fn delivery_only(sender: Arc<dyn AlertSender>) -> Self {
        Self {
            sender,
            updater: None,
        }
    }

    pub fn chat_provider<P>(provider: Arc<P>) -> Self
    where
        P: AlertSender + AlertMessageUpdater + 'static,
    {
        Self {
            sender: provider.clone(),
            updater: Some(provider),
        }
    }

    async fn deliver_batches(
        self,
        integration: IntegrationId,
        mut batches: Receiver<IntegrationBatch>,
    ) {
        while let Some(batch) = batches.recv().await {
            self.send_alerts(batch.alerts).await;
            self.mark_unbookable(&integration, &batch.taken).await;
        }
    }

    async fn send_alerts(&self, alerts: Vec<SubscriberAlert>) {
        stream::iter(alerts)
            .for_each_concurrent(MAX_CONCURRENT_ALERTS, |(subscriber, alert)| async move {
                if let Err(e) = self
                    .sender
                    .send_alert(&subscriber.subscriber_id, &alert)
                    .await
                {
                    warn!(
                        error = %format!("{e:#}"),
                        integration = %subscriber.integration,
                        subscriber = %subscriber.subscriber_id,
                        "dispatch: failed to send alert"
                    );
                }
            })
            .await;
    }

    async fn mark_unbookable(&self, integration: &IntegrationId, taken: &[BookableSlotId]) {
        if taken.is_empty() {
            return;
        }
        let Some(updater) = &self.updater else {
            return;
        };
        if let Err(e) = updater.mark_unbookable(taken).await {
            warn!(
                error = %format!("{e:#}"),
                integration = %integration,
                slots = taken.len(),
                "dispatch: failed to mark slots that stopped being bookable"
            );
        }
    }
}

type SubscriberAlert = (SubscriberRef, AvailabilityAlert);

struct IntegrationBatch {
    alerts: Vec<SubscriberAlert>,
    taken: Vec<BookableSlotId>,
}

struct IntegrationQueue {
    integration: IntegrationId,
    updates_messages: bool,
    batches: Sender<IntegrationBatch>,
}

impl IntegrationQueue {
    fn hand_off(&self, alerts: Vec<SubscriberAlert>, taken: &[BookableSlotId]) {
        let batch = IntegrationBatch {
            alerts,
            taken: if self.updates_messages {
                taken.to_vec()
            } else {
                Vec::new()
            },
        };
        if batch.alerts.is_empty() && batch.taken.is_empty() {
            return;
        }
        if let Err(rejected) = self.batches.try_send(batch) {
            let dropped = rejected.into_inner();
            warn!(
                error = "the integration's delivery queue is full",
                integration = %self.integration,
                alerts = dropped.alerts.len(),
                unbookable_slots = dropped.taken.len(),
                "dispatch: dropped a batch"
            );
        }
    }
}

pub struct AlertDispatcher {
    store: Arc<dyn SubscriptionRepository>,
    registry: Arc<RwLock<VenueRegistry>>,
    routes: HashMap<IntegrationId, AlertRoute>,
    clock: Arc<dyn Clock>,
}

impl AlertDispatcher {
    pub fn new(
        store: Arc<dyn SubscriptionRepository>,
        registry: Arc<RwLock<VenueRegistry>>,
        routes: HashMap<IntegrationId, AlertRoute>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            registry,
            routes,
            clock,
        }
    }

    pub async fn dispatch_loop(self: Arc<Self>, mut rx: Receiver<Vec<AvailabilityChange>>) {
        let (queues, mut workers) = self.start_workers();
        let mut last_expiry_cleanup = None;
        loop {
            tokio::select! {
                changes = rx.recv() => {
                    let Some(changes) = changes else { break };
                    self.cleanup_expired_if_needed(&mut last_expiry_cleanup)
                        .await;
                    self.hand_off(&changes, &queues).await;
                }
                Some(ended) = workers.join_next() => resume_if_panicked(ended),
            }
        }
        drop(queues);
        while let Some(ended) = workers.join_next().await {
            resume_if_panicked(ended);
        }
    }

    fn start_workers(&self) -> (Vec<IntegrationQueue>, JoinSet<()>) {
        let mut workers = JoinSet::new();
        let queues = self
            .routes
            .iter()
            .map(|(integration, route)| {
                let (batches, queued) = mpsc::channel(MAX_QUEUED_BATCHES);
                workers.spawn(route.clone().deliver_batches(integration.clone(), queued));
                IntegrationQueue {
                    integration: integration.clone(),
                    updates_messages: route.updater.is_some(),
                    batches,
                }
            })
            .collect();
        (queues, workers)
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

    async fn hand_off(&self, changes: &[AvailabilityChange], queues: &[IntegrationQueue]) {
        if changes.is_empty() {
            return;
        }
        let mut alerts = self.alerts_by_integration(changes).await;
        let taken = AvailabilityChange::taken_ids(changes);
        for queue in queues {
            let alerts = alerts.remove(&queue.integration).unwrap_or_default();
            queue.hand_off(alerts, &taken);
        }
    }

    async fn alerts_by_integration(
        &self,
        changes: &[AvailabilityChange],
    ) -> HashMap<IntegrationId, Vec<SubscriberAlert>> {
        let mut by_integration: HashMap<IntegrationId, Vec<SubscriberAlert>> = HashMap::new();
        let subs = match self.store.list_all(self.clock.today()).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %format!("{e:#}"), "dispatch: failed to load subscriptions");
                return by_integration;
            }
        };
        if subs.is_empty() {
            return by_integration;
        }
        let matched = {
            let registry = self.registry.read().expect("venue registry poisoned");
            match_subscriptions(changes, &subs, &registry)
        };
        let registry = self.registry.read().expect("venue registry poisoned");
        for (subscriber, slots) in matched {
            if !self.routes.contains_key(&subscriber.integration) {
                warn!(
                    integration = %subscriber.integration,
                    subscriber = %subscriber.subscriber_id,
                    "dispatch: no alert route for integration"
                );
                continue;
            }
            let alert = AvailabilityAlert {
                slots: slots
                    .iter()
                    .map(|slot| slot_summary(&registry, slot))
                    .collect(),
            };
            by_integration
                .entry(subscriber.integration.clone())
                .or_default()
                .push((subscriber, alert));
        }
        by_integration
    }
}

fn resume_if_panicked(ended: Result<(), JoinError>) {
    if let Err(error) = ended
        && error.is_panic()
    {
        std::panic::resume_unwind(error.into_panic());
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{
        INDOOR_COURT, PADEL_CLUB_NAME, RecordingSender, chat_route, court_id, delivery_route,
        dispatcher_on, padel_club, padel_court, padel_court_id, padel_venue_id, registry,
        registry_of, service_and_dispatcher, service_and_dispatcher_over, service_on, store,
        subscribe_request, subscribe_to_courts, tuesday_slot, uref, venue_id,
    };
    use super::{MAX_CONCURRENT_ALERTS, MAX_QUEUED_BATCHES};
    use crate::model::{
        AvailabilityChange, BookableSlot, BookableSlotId, CourtCatalog, CourtFilter,
        CourtSelection, Schedule, Sport, SubscriberRef, SubscriptionDraft, TimeRange,
        UncheckedDraft, VenueId, VenueRegistry,
    };
    use crate::ports::SubscriptionRepository;
    use crate::subscriptions::contract::{
        AlertMessageUpdater, AlertSender, AvailabilityAlert, CourtChoice, SubscriptionRequest,
    };
    use crate::subscriptions::{AlertDispatcher, SubscriptionService};
    use crate::time::today_berlin;
    use chrono::{TimeZone, Utc};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, RwLock};
    use std::time::Duration;
    use tokio::sync::{mpsc, watch};
    use tokio::task::JoinHandle;
    use tracing::field::Field;
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::{Context, SubscriberExt as _};
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
        let (svc, dispatcher) = service_and_dispatcher([delivery_route(
            "discord",
            Arc::new(RendezvousSender(tokio::sync::Barrier::new(2))),
        )])
        .await;
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
        let sender = RecordingSender::new(false);
        let (svc, dispatcher) =
            service_and_dispatcher([delivery_route("discord", sender.clone())]).await;
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
        let sender = RecordingSender::new(false);
        let (svc, dispatcher) =
            service_and_dispatcher([delivery_route("discord", sender.clone())]).await;
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
        let sender = RecordingSender::new(false);
        let (svc, dispatcher) =
            service_and_dispatcher([delivery_route("discord", sender.clone())]).await;
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
        let sender = RecordingSender::new(false);
        let (svc, dispatcher) =
            service_and_dispatcher([delivery_route("discord", sender.clone())]).await;
        svc.subscribe(&uref("1"), &subscribe_request(8 * 60, 10 * 60))
            .await
            .unwrap()
            .unwrap();

        dispatcher.dispatch(&[bookable("Court 2")]).await;

        assert!(sender.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dispatch_failing_send_does_not_abort_batch() {
        let sender = RecordingSender::new(true); // every send errors
        let (svc, dispatcher) =
            service_and_dispatcher([delivery_route("discord", sender.clone())]).await;
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
    async fn dispatch_without_a_route_does_not_panic() {
        let (svc, dispatcher) = service_and_dispatcher([]).await;
        svc.subscribe(&uref("1"), &subscribe_request(18 * 60, 22 * 60))
            .await
            .unwrap()
            .unwrap();
        dispatcher.dispatch(&[bookable("Court 2")]).await;
    }

    #[tokio::test]
    async fn a_subscriber_whose_integration_has_no_route_is_skipped_and_the_others_are_alerted() {
        let sender = RecordingSender::new(false);
        let (svc, dispatcher) =
            service_and_dispatcher([delivery_route("discord", sender.clone())]).await;
        let left_behind = SubscriberRef {
            integration: "webhook".into(),
            subscriber_id: "agent".into(),
        };
        for subscriber in [left_behind, uref("1")] {
            svc.subscribe(&subscriber, &subscribe_request(18 * 60, 22 * 60))
                .await
                .unwrap()
                .unwrap();
        }

        dispatcher.dispatch(&[bookable("Court 2")]).await;

        let sent = sender.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "1");
    }

    #[tokio::test]
    async fn a_route_delivers_from_the_moment_the_dispatcher_is_constructed() {
        let store = store().await;
        let registry = registry();
        service_on(store.clone(), registry.clone())
            .subscribe(&uref("1"), &subscribe_request(18 * 60, 22 * 60))
            .await
            .unwrap()
            .unwrap();
        let sender = RecordingSender::new(false);

        dispatcher_on(store, registry, [delivery_route("discord", sender.clone())])
            .dispatch(&[bookable("Court 2")])
            .await;

        assert_eq!(sender.sent.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_slot_that_stops_being_bookable_is_struck_where_it_was_announced() {
        let sender = RecordingSender::new(false);
        let (_, dispatcher) = service_and_dispatcher([chat_route("discord", sender.clone())]).await;
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
        let sender = RecordingSender::new(false);
        let (_, dispatcher) = service_and_dispatcher([chat_route("discord", sender.clone())]).await;
        let taken = unbookable("Court 2");

        dispatcher.dispatch(std::slice::from_ref(&taken)).await;

        assert_eq!(sender.struck(), vec![BookableSlotId::from(taken.slot())]);
    }

    #[tokio::test]
    async fn alerts_go_out_before_anything_is_struck() {
        let sender = RecordingSender::new(false);
        let (svc, dispatcher) =
            service_and_dispatcher([chat_route("discord", sender.clone())]).await;
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
        let sender = RecordingSender::new(false);
        let (svc, dispatcher) =
            service_and_dispatcher([chat_route("discord", sender.clone())]).await;
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
        let sender = RecordingSender::new(true);
        let (svc, dispatcher) =
            service_and_dispatcher([chat_route("discord", sender.clone())]).await;
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
        let first = RecordingSender::new(true);
        let second = RecordingSender::new(true);
        let (_, dispatcher) = service_and_dispatcher([
            chat_route("discord", first.clone()),
            chat_route("slack", second.clone()),
        ])
        .await;

        dispatcher.dispatch(&[unbookable("Court 2")]).await;

        assert_eq!(first.struck().len(), 1);
        assert_eq!(second.struck().len(), 1);
    }

    #[tokio::test]
    async fn a_delivery_only_route_is_alerted_but_never_asked_to_update() {
        let sender = RecordingSender::new(false);
        let (svc, dispatcher) =
            service_and_dispatcher([delivery_route("discord", sender.clone())]).await;
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
        let chat = RecordingSender::new(false);
        let delivery_only = RecordingSender::new(false);
        let (_, dispatcher) = service_and_dispatcher([
            chat_route("discord", chat.clone()),
            delivery_route("webhook", delivery_only.clone()),
        ])
        .await;
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
        let sender = RecordingSender::new(false);
        let (svc, dispatcher) = service_and_dispatcher_over(
            registry.clone(),
            [delivery_route("discord", sender.clone())],
        )
        .await;
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
        let sender = RecordingSender::new(false);
        let (svc, dispatcher) = service_and_dispatcher_over(
            registry.clone(),
            [delivery_route("discord", sender.clone())],
        )
        .await;
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

    fn expired_subscription() -> SubscriptionDraft {
        UncheckedDraft {
            subscriber: uref("1"),
            sport: Sport::Tennis,
            venue: None,
            schedule: Schedule::Date(chrono::NaiveDate::from_ymd_opt(2000, 1, 1).unwrap()),
            time_range: TimeRange::new(0, 24 * 60).unwrap(),
            courts: CourtSelection::Every,
            filter: CourtFilter::Any,
        }
        .assume_valid()
    }

    #[tokio::test]
    async fn daily_cleanup_removes_expired_date_subscriptions() {
        let store = store().await;
        let dispatcher = dispatcher_on(store.clone(), registry(), []);
        store.add(expired_subscription()).await.unwrap();
        let before_expiry = chrono::NaiveDate::from_ymd_opt(1900, 1, 1).unwrap();
        assert_eq!(store.list_all(before_expiry).await.unwrap().len(), 1);

        let mut last_cleanup = None;
        dispatcher
            .cleanup_expired_if_needed(&mut last_cleanup)
            .await;

        assert_eq!(store.list_all(before_expiry).await.unwrap().len(), 0);
        assert_eq!(last_cleanup, Some(today_berlin()));

        store.add(expired_subscription()).await.unwrap();
        dispatcher
            .cleanup_expired_if_needed(&mut last_cleanup)
            .await;
        assert_eq!(store.list_all(before_expiry).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_dispatch_loop_removes_expired_date_subscriptions() {
        let store = store().await;
        store.add(expired_subscription()).await.unwrap();
        let before_expiry = chrono::NaiveDate::from_ymd_opt(1900, 1, 1).unwrap();

        dispatcher_on(store.clone(), registry(), [])
            .dispatch(&[bookable("Court 2")])
            .await;

        assert!(store.list_all(before_expiry).await.unwrap().is_empty());
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Finished {
        Alert(String),
        Update(Vec<BookableSlotId>),
    }

    /// Holds the alerts `holds` picks out until `release`, and records each step as it finishes.
    struct GatedProvider {
        holds: fn(&str, &str) -> bool,
        gate: watch::Sender<bool>,
        started: AtomicUsize,
        finished: Mutex<Vec<Finished>>,
    }

    impl GatedProvider {
        fn holding(holds: fn(&str, &str) -> bool) -> Arc<Self> {
            Arc::new(Self {
                holds,
                gate: watch::Sender::new(false),
                started: AtomicUsize::new(0),
                finished: Mutex::new(Vec::new()),
            })
        }

        fn holding_every_alert() -> Arc<Self> {
            Self::holding(|_, _| true)
        }

        fn release(&self) {
            self.gate.send_replace(true);
        }

        fn started(&self) -> usize {
            self.started.load(Ordering::SeqCst)
        }

        fn finished(&self) -> Vec<Finished> {
            self.finished.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl AlertSender for GatedProvider {
        async fn send_alert(
            &self,
            subscriber: &str,
            alert: &AvailabilityAlert,
        ) -> anyhow::Result<()> {
            let court = alert.slots[0].court.clone();
            self.started.fetch_add(1, Ordering::SeqCst);
            if (self.holds)(subscriber, &court) {
                self.gate.subscribe().wait_for(|open| *open).await?;
            }
            self.finished.lock().unwrap().push(Finished::Alert(court));
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl AlertMessageUpdater for GatedProvider {
        async fn mark_unbookable(&self, slots: &[BookableSlotId]) -> anyhow::Result<()> {
            self.finished
                .lock()
                .unwrap()
                .push(Finished::Update(slots.to_vec()));
            Ok(())
        }
    }

    struct PanickingSender;

    #[async_trait::async_trait]
    impl AlertSender for PanickingSender {
        async fn send_alert(&self, _: &str, _: &AvailabilityAlert) -> anyhow::Result<()> {
            panic!("simulated alert panic")
        }
    }

    #[derive(Clone, Default)]
    struct Warnings(Arc<Mutex<Vec<String>>>);

    impl Warnings {
        fn logged(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    impl<S: tracing::Subscriber> Layer<S> for Warnings {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            if event.metadata().level() == &tracing::Level::WARN {
                let mut fields = String::new();
                event.record(&mut |field: &Field, value: &dyn std::fmt::Debug| {
                    fields.push_str(&format!("{field}={value:?} "));
                });
                self.0.lock().unwrap().push(fields);
            }
        }
    }

    fn capture_warnings() -> (Warnings, tracing::subscriber::DefaultGuard) {
        let warnings = Warnings::default();
        let guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(warnings.clone()));
        (warnings, guard)
    }

    fn subscriber_of(integration: &'static str, id: &str) -> SubscriberRef {
        SubscriberRef {
            integration: integration.into(),
            subscriber_id: id.into(),
        }
    }

    async fn subscribe_all(
        svc: &SubscriptionService,
        subscribers: impl IntoIterator<Item = SubscriberRef>,
    ) {
        for subscriber in subscribers {
            svc.subscribe(&subscriber, &subscribe_request(18 * 60, 22 * 60))
                .await
                .unwrap()
                .unwrap();
        }
    }

    fn numbered_batch(n: usize) -> Vec<AvailabilityChange> {
        vec![
            unbookable(&format!("Taken {n}")),
            bookable(&format!("Open {n}")),
        ]
    }

    fn alert_of(n: usize) -> Finished {
        Finished::Alert(format!("Open {n}"))
    }

    fn update_of(batch: &[AvailabilityChange]) -> Finished {
        Finished::Update(AvailabilityChange::taken_ids(batch))
    }

    fn steps_of(batches: &[Vec<AvailabilityChange>]) -> Vec<Finished> {
        batches
            .iter()
            .enumerate()
            .flat_map(|(n, batch)| [alert_of(n), update_of(batch)])
            .collect()
    }

    const PATIENCE: Duration = Duration::from_secs(60);

    fn running(
        dispatcher: Arc<AlertDispatcher>,
    ) -> (mpsc::Sender<Vec<AvailabilityChange>>, JoinHandle<()>) {
        let (input, rx) = mpsc::channel(2);
        (input, tokio::spawn(dispatcher.dispatch_loop(rx)))
    }

    /// In paused time, this returns once every other task has run as far as it can.
    async fn settle() {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    async fn publish(
        input: &mpsc::Sender<Vec<AvailabilityChange>>,
        batch: Vec<AvailabilityChange>,
    ) {
        tokio::time::timeout(PATIENCE, input.send(batch))
            .await
            .expect("the dispatch loop stopped taking batches")
            .unwrap();
        settle().await;
    }

    async fn finishes(dispatch: JoinHandle<()>) {
        tokio::time::timeout(PATIENCE, dispatch)
            .await
            .expect("the dispatch loop never finished")
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_saturated_integration_holds_back_neither_alerts_nor_updates_of_a_chat_provider() {
        let stalled = GatedProvider::holding_every_alert();
        let chat = RecordingSender::new(false);
        let (svc, dispatcher) = service_and_dispatcher([
            delivery_route("webhook", stalled.clone()),
            chat_route("discord", chat.clone()),
        ])
        .await;
        let agents = (0..=MAX_CONCURRENT_ALERTS)
            .map(|agent| subscriber_of("webhook", &agent.to_string()))
            .collect::<Vec<_>>();
        subscribe_all(&svc, agents).await;
        subscribe_all(&svc, [uref("1")]).await;
        let (input, _dispatch) = running(dispatcher);
        let batches = [numbered_batch(0), numbered_batch(1)];

        publish(&input, batches[0].clone()).await;
        assert_eq!(
            stalled.started(),
            MAX_CONCURRENT_ALERTS,
            "the webhook-like route should be using all of its own concurrency"
        );
        publish(&input, batches[1].clone()).await;

        assert_eq!(chat.order(), ["send", "strike", "send", "strike"]);
        assert_eq!(
            chat.struck(),
            batches
                .iter()
                .flat_map(|batch| AvailabilityChange::taken_ids(batch))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_overflowing_integration_drops_new_batches_while_the_others_get_every_batch() {
        let stalled = GatedProvider::holding_every_alert();
        let responsive = RecordingSender::new(false);
        let (svc, dispatcher) = service_and_dispatcher([
            chat_route("stalled", stalled.clone()),
            chat_route("discord", responsive.clone()),
        ])
        .await;
        subscribe_all(&svc, [subscriber_of("stalled", "1"), uref("1")]).await;
        let (warnings, _logging) = capture_warnings();
        let (input, dispatch) = running(dispatcher);
        let in_flight_or_queued = 1 + MAX_QUEUED_BATCHES;
        let dropped = 3;
        let batches: Vec<_> = (0..in_flight_or_queued + dropped)
            .map(numbered_batch)
            .collect();

        for batch in &batches {
            publish(&input, batch.clone()).await;
        }

        assert_eq!(stalled.started(), 1);
        let alerted: Vec<_> = responsive
            .sent
            .lock()
            .unwrap()
            .iter()
            .map(|(_, alert)| Finished::Alert(alert.slots[0].court.clone()))
            .collect();
        assert_eq!(
            alerted,
            (0..batches.len()).map(alert_of).collect::<Vec<_>>()
        );
        assert_eq!(
            responsive.struck(),
            batches
                .iter()
                .flat_map(|batch| AvailabilityChange::taken_ids(batch))
                .collect::<Vec<_>>()
        );
        let logged = warnings.logged();
        assert_eq!(logged.len(), dropped, "{logged:?}");
        for warning in logged {
            for named in [
                "error=",
                "integration=stalled",
                "alerts=1",
                "unbookable_slots=1",
            ] {
                assert!(warning.contains(named), "{warning} lacks {named}");
            }
        }

        stalled.release();
        drop(input);
        finishes(dispatch).await;

        assert_eq!(
            stalled.finished(),
            steps_of(&batches[..in_flight_or_queued])
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_integration_finishes_each_batch_in_order_and_alerts_before_updates() {
        let provider =
            GatedProvider::holding(|subscriber, court| subscriber == "held" && court == "Open 0");
        let (svc, dispatcher) =
            service_and_dispatcher([chat_route("discord", provider.clone())]).await;
        subscribe_all(&svc, [uref("held"), uref("free")]).await;
        let (first, second) = (numbered_batch(0), numbered_batch(1));
        let (input, dispatch) = running(dispatcher);

        publish(&input, first.clone()).await;
        publish(&input, second.clone()).await;

        assert_eq!(
            provider.finished(),
            [alert_of(0)],
            "one alert of the first batch is held, so nothing after it may finish"
        );

        provider.release();
        drop(input);
        finishes(dispatch).await;

        assert_eq!(
            provider.finished(),
            [
                alert_of(0),
                alert_of(0),
                update_of(&first),
                alert_of(1),
                alert_of(1),
                update_of(&second),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn closing_the_input_finishes_pending_batches_before_the_loop_ends() {
        let held = GatedProvider::holding_every_alert();
        let (svc, dispatcher) = service_and_dispatcher([chat_route("discord", held.clone())]).await;
        subscribe_all(&svc, [uref("1")]).await;
        let batches = [numbered_batch(0), numbered_batch(1)];
        let (input, dispatch) = running(dispatcher);
        for batch in &batches {
            publish(&input, batch.clone()).await;
        }

        drop(input);
        settle().await;

        assert!(
            !dispatch.is_finished(),
            "the loop ended with one batch in flight and another queued"
        );

        held.release();
        finishes(dispatch).await;

        assert_eq!(held.finished(), steps_of(&batches));
    }

    #[tokio::test(start_paused = true)]
    async fn a_batch_with_nothing_for_an_integration_takes_no_place_in_its_queue() {
        let stalled = GatedProvider::holding_every_alert();
        let (svc, dispatcher) =
            service_and_dispatcher([delivery_route("webhook", stalled.clone())]).await;
        subscribe_all(&svc, [subscriber_of("webhook", "agent")]).await;
        let (warnings, _logging) = capture_warnings();
        let (input, _dispatch) = running(dispatcher);

        publish(&input, vec![bookable("Court 2")]).await;
        for _ in 0..=MAX_QUEUED_BATCHES {
            publish(&input, vec![unbookable("Court 5")]).await;
        }

        assert_eq!(warnings.logged(), Vec::<String>::new());
    }

    #[tokio::test(start_paused = true)]
    async fn a_panicking_worker_ends_the_dispatch_loop_with_its_input_still_open() {
        let (svc, dispatcher) =
            service_and_dispatcher([delivery_route("discord", Arc::new(PanickingSender))]).await;
        subscribe_all(&svc, [uref("1")]).await;
        let (input, dispatch) = running(dispatcher);

        publish(&input, vec![bookable("Court 2")]).await;

        let ended = tokio::time::timeout(PATIENCE, dispatch)
            .await
            .expect("the panic should end the loop, as it did when it delivered inline");
        assert!(ended.unwrap_err().is_panic());
    }
}
