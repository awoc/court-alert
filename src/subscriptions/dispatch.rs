use std::sync::Arc;

use tokio::sync::mpsc::Receiver;
use tracing::warn;

use crate::model::{AvailabilityChange, BookableSlotId};
use crate::subscriptions::contract::{AvailabilityAlert, DirectMessageSender};

use super::matcher::match_subscriptions;
use super::{SubscriptionService, slot_summary};

impl SubscriptionService {
    pub async fn dispatch_loop(
        self: std::sync::Arc<Self>,
        mut rx: Receiver<Vec<AvailabilityChange>>,
    ) {
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
        self.strike_taken(changes).await;

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
        for (user, slots) in matched {
            let alert = {
                let registry = self.registry.read().expect("venue registry poisoned");
                AvailabilityAlert {
                    slots: slots
                        .iter()
                        .map(|slot| slot_summary(&registry, slot))
                        .collect(),
                }
            };
            let sender = self
                .senders
                .read()
                .expect("senders lock poisoned")
                .get(&user.provider)
                .cloned();
            let Some(sender) = sender else {
                warn!(
                    provider = %user.provider,
                    user = %user.user_id,
                    "dispatch: no sender registered for provider"
                );
                continue;
            };
            if let Err(e) = sender.send_dm(&user.user_id, &alert).await {
                warn!(
                    error = %format!("{e:#}"),
                    provider = %user.provider,
                    user = %user.user_id,
                    "dispatch: failed to send DM"
                );
            }
        }
    }

    async fn strike_taken(&self, changes: &[AvailabilityChange]) {
        let taken: Vec<BookableSlotId> = changes
            .iter()
            .filter_map(|change| match change {
                AvailabilityChange::BecameUnbookable(slot) => Some(BookableSlotId::from(slot)),
                AvailabilityChange::BecameBookable(_) => None,
            })
            .collect();
        if taken.is_empty() {
            return;
        }
        let senders: Vec<(String, Arc<dyn DirectMessageSender>)> = self
            .senders
            .read()
            .expect("senders lock poisoned")
            .iter()
            .map(|(provider, sender)| (provider.clone(), sender.clone()))
            .collect();
        for (provider, sender) in senders {
            if let Err(e) = sender.strike_taken(&taken).await {
                warn!(
                    error = %format!("{e:#}"),
                    provider = %provider,
                    slots = taken.len(),
                    "dispatch: failed to strike slots that were taken"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{
        INDOOR_COURT, PADEL_CLUB_NAME, court_id, padel_court_id, padel_venue_id, service,
        service_with_store, subscribe_cmd, uref, venue_id,
    };
    use crate::model::{
        AvailabilityChange, BookableSlot, BookableSlotId, CourtFilter, Schedule, Sport,
        SubscriptionDraft, TimeRange,
    };
    use crate::ports::SubscriptionRepository;
    use crate::subscriptions::contract::{
        AvailabilityAlert, DirectMessageSender, SubscriptionCommand,
    };
    use crate::time::today_berlin;
    use chrono::{TimeZone, Utc};
    use std::sync::{Arc, Mutex};

    struct RecordingSender {
        sent: Mutex<Vec<(String, AvailabilityAlert)>>,
        struck: Mutex<Vec<Vec<BookableSlotId>>>,
        fail: bool,
    }

    impl RecordingSender {
        fn new(fail: bool) -> Arc<Self> {
            Arc::new(Self {
                sent: Mutex::new(Vec::new()),
                struck: Mutex::new(Vec::new()),
                fail,
            })
        }

        fn struck(&self) -> Vec<BookableSlotId> {
            self.struck.lock().unwrap().concat()
        }
    }

    #[async_trait::async_trait]
    impl DirectMessageSender for RecordingSender {
        async fn send_dm(&self, user_id: &str, alert: &AvailabilityAlert) -> anyhow::Result<()> {
            self.sent
                .lock()
                .unwrap()
                .push((user_id.to_string(), alert.clone()));
            if self.fail {
                anyhow::bail!("simulated DM failure");
            }
            Ok(())
        }

        async fn strike_taken(&self, slots: &[BookableSlotId]) -> anyhow::Result<()> {
            self.struck.lock().unwrap().push(slots.to_vec());
            if self.fail {
                anyhow::bail!("simulated strike failure");
            }
            Ok(())
        }
    }

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

    #[tokio::test]
    async fn dispatch_sends_dm_to_matching_subscriber() {
        let svc = service().await;
        let sender = RecordingSender::new(false);
        svc.register_sender("discord", sender.clone());
        svc.handle(&uref("1"), subscribe_cmd(18 * 60, 22 * 60))
            .await
            .unwrap();

        svc.dispatch(&[bookable("Court 2")]).await;

        let sent = sender.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "1");
        assert_eq!(sent[0].1.slots.len(), 1);
        assert_eq!(sent[0].1.slots[0].court, "Court 2");
    }

    #[tokio::test]
    async fn an_alert_names_the_club_each_court_belongs_to() {
        let svc = service().await;
        let sender = RecordingSender::new(false);
        svc.register_sender("discord", sender.clone());
        svc.handle(&uref("1"), subscribe_cmd(18 * 60, 22 * 60))
            .await
            .unwrap();

        svc.dispatch(&[bookable("Court 2")]).await;

        let sent = sender.sent.lock().unwrap();
        assert_eq!(sent[0].1.slots[0].club, "ZHS München");
    }

    #[tokio::test]
    async fn courts_of_the_same_name_at_different_clubs_are_distinguishable() {
        let svc = service().await;
        let sender = RecordingSender::new(false);
        svc.register_sender("discord", sender.clone());
        svc.handle(
            &uref("1"),
            SubscriptionCommand::Subscribe {
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
        .unwrap();

        svc.dispatch(&[padel_bookable()]).await;

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
        let svc = service().await;
        let sender = RecordingSender::new(false);
        svc.register_sender("discord", sender.clone());
        svc.handle(&uref("1"), subscribe_cmd(8 * 60, 10 * 60))
            .await
            .unwrap();

        svc.dispatch(&[bookable("Court 2")]).await;

        assert!(sender.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dispatch_failing_send_does_not_abort_batch() {
        let svc = service().await;
        let sender = RecordingSender::new(true); // every send errors
        svc.register_sender("discord", sender.clone());
        svc.handle(&uref("1"), subscribe_cmd(18 * 60, 22 * 60))
            .await
            .unwrap();
        svc.handle(&uref("2"), subscribe_cmd(18 * 60, 22 * 60))
            .await
            .unwrap();

        svc.dispatch(&[bookable("Court 2")]).await;

        assert_eq!(sender.sent.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn dispatch_without_registered_sender_does_not_panic() {
        let svc = service().await;
        svc.handle(&uref("1"), subscribe_cmd(18 * 60, 22 * 60))
            .await
            .unwrap();
        svc.dispatch(&[bookable("Court 2")]).await;
    }

    #[tokio::test]
    async fn a_slot_that_stops_being_bookable_is_struck_where_it_was_announced() {
        let svc = service().await;
        let sender = RecordingSender::new(false);
        svc.register_sender("discord", sender.clone());
        let taken = unbookable("Court 2");

        svc.dispatch(std::slice::from_ref(&taken)).await;

        assert_eq!(sender.struck(), vec![BookableSlotId::from(taken.slot())]);
        assert!(
            sender.sent.lock().unwrap().is_empty(),
            "a court going away is not news worth a fresh alert"
        );
    }

    #[tokio::test]
    async fn slots_are_struck_even_when_no_subscription_matches_them() {
        let svc = service().await;
        let sender = RecordingSender::new(false);
        svc.register_sender("discord", sender.clone());
        let taken = unbookable("Court 2");

        svc.dispatch(std::slice::from_ref(&taken)).await;

        assert_eq!(sender.struck(), vec![BookableSlotId::from(taken.slot())]);
    }

    #[tokio::test]
    async fn a_batch_of_new_slots_strikes_nothing() {
        let svc = service().await;
        let sender = RecordingSender::new(false);
        svc.register_sender("discord", sender.clone());
        svc.handle(&uref("1"), subscribe_cmd(18 * 60, 22 * 60))
            .await
            .unwrap();

        svc.dispatch(&[bookable("Court 2")]).await;

        assert!(sender.struck().is_empty());
        assert_eq!(sender.sent.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_failing_strike_does_not_stop_the_alerts_in_the_same_batch() {
        let svc = service().await;
        let sender = RecordingSender::new(true);
        svc.register_sender("discord", sender.clone());
        svc.handle(&uref("1"), subscribe_cmd(18 * 60, 22 * 60))
            .await
            .unwrap();

        svc.dispatch(&[unbookable("Court 5"), bookable("Court 2")])
            .await;

        assert_eq!(sender.struck().len(), 1);
        assert_eq!(sender.sent.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn daily_cleanup_removes_expired_date_subscriptions() {
        let (svc, store) = service_with_store().await;
        store
            .add(SubscriptionDraft {
                user: uref("1"),
                sport: Sport::Tennis,
                venue: None,
                schedule: Schedule::Date(chrono::NaiveDate::from_ymd_opt(2000, 1, 1).unwrap()),
                time_range: TimeRange::new(0, 24 * 60).unwrap(),
                courts: None,
                filter: CourtFilter::Any,
            })
            .await
            .unwrap();
        let before_expiry = chrono::NaiveDate::from_ymd_opt(1900, 1, 1).unwrap();
        assert_eq!(store.list_all(before_expiry).await.unwrap().len(), 1);

        let mut last_cleanup = None;
        svc.cleanup_expired_if_needed(&mut last_cleanup).await;

        assert_eq!(store.list_all(before_expiry).await.unwrap().len(), 0);
        assert_eq!(last_cleanup, Some(today_berlin()));

        store
            .add(SubscriptionDraft {
                user: uref("1"),
                sport: Sport::Tennis,
                venue: None,
                schedule: Schedule::Date(chrono::NaiveDate::from_ymd_opt(2000, 1, 1).unwrap()),
                time_range: TimeRange::new(0, 24 * 60).unwrap(),
                courts: None,
                filter: CourtFilter::Any,
            })
            .await
            .unwrap();
        svc.cleanup_expired_if_needed(&mut last_cleanup).await;
        assert_eq!(store.list_all(before_expiry).await.unwrap().len(), 1);
    }
}
