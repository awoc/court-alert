use tokio::sync::mpsc::Receiver;
use tracing::warn;

use crate::model::AvailabilityChange;
use crate::subscriptions::contract::AvailabilityAlert;

use super::SubscriptionService;
use super::matcher::match_subscriptions;

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
            let alert = AvailabilityAlert {
                slots: slots.iter().map(Into::into).collect(),
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
}

#[cfg(test)]
mod tests {
    use super::super::testing::{
        court_id, service, service_with_store, subscribe_cmd, uref, venue_id,
    };
    use crate::model::{
        AvailabilityChange, BookableSlot, CourtFilter, Schedule, SubscriptionDraft, TimeRange,
    };
    use crate::ports::SubscriptionRepository;
    use crate::subscriptions::contract::{AvailabilityAlert, DirectMessageSender};
    use crate::time::today_berlin;
    use chrono::{TimeZone, Utc};
    use std::sync::{Arc, Mutex};

    struct RecordingSender {
        sent: Mutex<Vec<(String, AvailabilityAlert)>>,
        fail: bool,
    }

    impl RecordingSender {
        fn new(fail: bool) -> Arc<Self> {
            Arc::new(Self {
                sent: Mutex::new(Vec::new()),
                fail,
            })
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
    async fn daily_cleanup_removes_expired_date_subscriptions() {
        let (svc, store) = service_with_store().await;
        store
            .add(SubscriptionDraft {
                user: uref("1"),
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
