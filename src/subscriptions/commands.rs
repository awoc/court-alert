use anyhow::Result;
use tracing::warn;

use crate::model::{
    BookableSlot, CourtFilter, ProviderUserRef, Schedule, Subscription, SubscriptionDraft,
    TimeRange,
};
use crate::subscriptions::contract::{
    AvailableSlotSummary, SubscriptionCommand, SubscriptionResult,
};

use super::SubscriptionService;
use super::matcher::slot_matches;

impl SubscriptionService {
    pub async fn handle(
        &self,
        user: &ProviderUserRef,
        command: SubscriptionCommand,
    ) -> Result<SubscriptionResult> {
        match command {
            SubscriptionCommand::Subscribe {
                schedule,
                start_minute,
                end_minute,
                courts,
                filter,
            } => {
                let Some(time_range) = TimeRange::new(start_minute, end_minute) else {
                    return Ok(SubscriptionResult::InvalidTimeRange);
                };
                if let Schedule::Date(d) = schedule
                    && d < self.clock.today()
                {
                    return Ok(SubscriptionResult::InvalidSchedule);
                }
                let courts = match self.canonicalize_courts(courts) {
                    Ok(courts) => courts,
                    Err(unknown) => {
                        return Ok(SubscriptionResult::UnknownCourts {
                            unknown,
                            available: self.court_names(),
                        });
                    }
                };
                let chosen_filter = filter;
                let filter = self.resolve_filter(filter, courts.as_deref());

                if let Some(chosen) = chosen_filter
                    && let Some(excluded) = self.courts_excluded_by(chosen, courts.as_deref())
                {
                    return Ok(SubscriptionResult::FilterExcludesCourts {
                        courts: excluded,
                        filter: chosen,
                    });
                }
                let id = self
                    .store
                    .add(SubscriptionDraft {
                        user: user.clone(),
                        schedule,
                        time_range,
                        courts: courts.clone(),
                        filter,
                    })
                    .await?;
                let sub = Subscription {
                    id,
                    user: user.clone(),
                    schedule,
                    time_range,
                    courts,
                    filter,
                };
                let open_slots = self.open_slots_for(&sub).await;
                Ok(SubscriptionResult::Subscribed {
                    summary: sub.into(),
                    open_slots,
                })
            }
            SubscriptionCommand::List => {
                let subs = self.store.list_for_user(user, self.clock.today()).await?;
                Ok(SubscriptionResult::SubscriptionList(
                    subs.into_iter().map(Into::into).collect(),
                ))
            }
            SubscriptionCommand::ListAll => {
                if !self.is_admin(user) {
                    return Ok(SubscriptionResult::NotAuthorized);
                }
                let subs = self.store.list_all(self.clock.today()).await?;
                Ok(SubscriptionResult::AllSubscriptions(
                    subs.into_iter().map(Into::into).collect(),
                ))
            }
            SubscriptionCommand::Unsubscribe { id } => {
                let removed = if self.is_admin(user) {
                    self.store.remove_any(id).await?
                } else {
                    self.store.remove(id, user).await?
                };
                if removed {
                    Ok(SubscriptionResult::Unsubscribed { id })
                } else {
                    Ok(SubscriptionResult::NotFound { id })
                }
            }
        }
    }

    fn canonicalize_courts(
        &self,
        courts: Option<Vec<String>>,
    ) -> Result<Option<Vec<String>>, Vec<String>> {
        let Some(courts) = courts else {
            return Ok(None);
        };
        let catalogs = self.tennis_catalogs();
        let mut canonical = Vec::with_capacity(courts.len());
        let mut unknown = Vec::new();
        for court in courts {
            match catalogs
                .iter()
                .find_map(|(_, catalog)| catalog.resolve(&court))
            {
                Some(known) => canonical.push(known.name().to_owned()),
                None => unknown.push(court),
            }
        }
        if unknown.is_empty() {
            Ok(Some(canonical))
        } else {
            Err(unknown)
        }
    }

    fn resolve_filter(
        &self,
        chosen: Option<CourtFilter>,
        courts: Option<&[String]>,
    ) -> CourtFilter {
        chosen.unwrap_or(match courts {
            // Naming courts means those courts, whatever they are made of.
            Some(_) => CourtFilter::Any,
            None => self.default_surface,
        })
    }

    fn courts_excluded_by(
        &self,
        filter: CourtFilter,
        courts: Option<&[String]>,
    ) -> Option<Vec<String>> {
        let catalogs = self.tennis_catalogs();
        let excluded: Vec<String> = courts?
            .iter()
            .filter(|name| {
                let attributes = catalogs
                    .iter()
                    .find_map(|(_, catalog)| catalog.find_by_name(name))
                    .map(|court| court.attributes().clone());
                !filter.allows(attributes.as_ref())
            })
            .cloned()
            .collect();
        (!excluded.is_empty()).then_some(excluded)
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
        slots.into_iter().map(Into::into).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{
        SYNTHETIC_COURT, admin_uref, date_subscribe_cmd, open_slot, service,
        service_defaulting_to_clay, service_with_admin, service_with_clock,
        service_with_failing_slot_snapshot, service_with_slots, subscribe_cmd, uref,
    };
    use crate::model::{CourtFilter, CourtSurface, Schedule, SubscriptionDraft, TimeRange};
    use crate::ports::SubscriptionRepository;
    use crate::subscriptions::contract::{SubscriptionCommand, SubscriptionResult};
    use chrono::Weekday;

    #[tokio::test]
    async fn subscribe_persists_and_returns_summary() {
        let svc = service().await;
        let reply = svc
            .handle(&uref("1"), subscribe_cmd(18 * 60, 20 * 60))
            .await
            .unwrap();
        let SubscriptionResult::Subscribed {
            summary,
            open_slots,
        } = reply
        else {
            panic!("expected Subscribed, got {reply:?}");
        };
        assert_eq!(summary.schedule, Schedule::Weekday(Weekday::Tue));
        assert_eq!(summary.time_range, TimeRange::new(1080, 1200).unwrap());
        assert!(open_slots.is_empty()); // empty snapshot → empty preview

        let list = svc
            .handle(&uref("1"), SubscriptionCommand::List)
            .await
            .unwrap();
        let SubscriptionResult::SubscriptionList(subs) = list else {
            panic!("expected SubscriptionList");
        };
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].id, summary.id);
    }

    #[tokio::test]
    async fn subscribe_rejects_inverted_time_range_and_stores_nothing() {
        let svc = service().await;
        let reply = svc
            .handle(&uref("1"), subscribe_cmd(20 * 60, 18 * 60))
            .await
            .unwrap();
        assert_eq!(reply, SubscriptionResult::InvalidTimeRange);

        let list = svc
            .handle(&uref("1"), SubscriptionCommand::List)
            .await
            .unwrap();
        assert_eq!(list, SubscriptionResult::SubscriptionList(vec![]));
    }

    #[tokio::test]
    async fn list_returns_only_own_subscriptions() {
        let svc = service().await;
        svc.handle(&uref("1"), subscribe_cmd(18 * 60, 20 * 60))
            .await
            .unwrap();
        svc.handle(&uref("2"), subscribe_cmd(8 * 60, 10 * 60))
            .await
            .unwrap();

        let SubscriptionResult::SubscriptionList(subs) = svc
            .handle(&uref("1"), SubscriptionCommand::List)
            .await
            .unwrap()
        else {
            panic!("expected SubscriptionList");
        };
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].time_range.start_minute(), 18 * 60);
    }

    #[tokio::test]
    async fn list_filters_expired_one_shot_subscriptions_without_cleanup_write() {
        let (svc, store) = super::super::testing::service_with_store().await;
        store
            .add(SubscriptionDraft {
                user: uref("1"),
                schedule: Schedule::Date(chrono::NaiveDate::from_ymd_opt(2000, 1, 1).unwrap()),
                time_range: TimeRange::new(18 * 60, 20 * 60).unwrap(),
                courts: None,
                filter: CourtFilter::Any,
            })
            .await
            .unwrap();

        let reply = svc
            .handle(&uref("1"), SubscriptionCommand::List)
            .await
            .unwrap();

        assert_eq!(reply, SubscriptionResult::SubscriptionList(vec![]));
        let before_expiry = chrono::NaiveDate::from_ymd_opt(1900, 1, 1).unwrap();
        assert_eq!(store.list_all(before_expiry).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unsubscribe_removes_own_and_rejects_foreign() {
        let svc = service().await;
        let SubscriptionResult::Subscribed { summary, .. } = svc
            .handle(&uref("1"), subscribe_cmd(18 * 60, 20 * 60))
            .await
            .unwrap()
        else {
            panic!("expected Subscribed");
        };

        let foreign = svc
            .handle(
                &uref("2"),
                SubscriptionCommand::Unsubscribe { id: summary.id },
            )
            .await
            .unwrap();
        assert_eq!(foreign, SubscriptionResult::NotFound { id: summary.id });

        let own = svc
            .handle(
                &uref("1"),
                SubscriptionCommand::Unsubscribe { id: summary.id },
            )
            .await
            .unwrap();
        assert_eq!(own, SubscriptionResult::Unsubscribed { id: summary.id });
    }

    #[tokio::test]
    async fn subscribe_rejects_equal_start_and_end() {
        let svc = service().await;
        let reply = svc
            .handle(&uref("1"), subscribe_cmd(18 * 60, 18 * 60))
            .await
            .unwrap();
        assert_eq!(reply, SubscriptionResult::InvalidTimeRange);
    }

    #[tokio::test]
    async fn subscribe_rejects_time_range_outside_one_day() {
        let svc = service().await;
        let reply = svc
            .handle(&uref("1"), subscribe_cmd(23 * 60, 25 * 60))
            .await
            .unwrap();
        assert_eq!(reply, SubscriptionResult::InvalidTimeRange);
    }

    #[tokio::test]
    async fn subscribe_rejects_past_date() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 15).unwrap();
        let svc = service_with_clock(today).await;
        let yesterday = today.pred_opt().unwrap();
        let reply = svc
            .handle(&uref("1"), date_subscribe_cmd(yesterday, 18 * 60, 20 * 60))
            .await
            .unwrap();
        assert_eq!(reply, SubscriptionResult::InvalidSchedule);
        assert_eq!(
            svc.handle(&uref("1"), SubscriptionCommand::List)
                .await
                .unwrap(),
            SubscriptionResult::SubscriptionList(vec![])
        );
    }

    #[tokio::test]
    async fn subscribe_accepts_todays_date() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 15).unwrap();
        let svc = service_with_clock(today).await;
        let reply = svc
            .handle(&uref("1"), date_subscribe_cmd(today, 18 * 60, 20 * 60))
            .await
            .unwrap();
        assert!(matches!(reply, SubscriptionResult::Subscribed { .. }));
    }

    #[tokio::test]
    async fn subscribe_accepts_far_future_date() {
        let svc = service().await;
        let far = chrono::NaiveDate::from_ymd_opt(2099, 12, 31).unwrap();
        let reply = svc
            .handle(&uref("1"), date_subscribe_cmd(far, 18 * 60, 20 * 60))
            .await
            .unwrap();
        assert!(matches!(reply, SubscriptionResult::Subscribed { .. }));
    }

    #[tokio::test]
    async fn listall_returns_everything_for_admin() {
        let svc = service_with_admin().await;
        svc.handle(&uref("1"), subscribe_cmd(18 * 60, 20 * 60))
            .await
            .unwrap();
        svc.handle(&uref("2"), subscribe_cmd(8 * 60, 10 * 60))
            .await
            .unwrap();

        let SubscriptionResult::AllSubscriptions(all) = svc
            .handle(&admin_uref(), SubscriptionCommand::ListAll)
            .await
            .unwrap()
        else {
            panic!("expected AllSubscriptions");
        };
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn listall_denied_for_non_admin() {
        let svc = service_with_admin().await;
        let reply = svc
            .handle(&uref("1"), SubscriptionCommand::ListAll)
            .await
            .unwrap();
        assert_eq!(reply, SubscriptionResult::NotAuthorized);
    }

    #[tokio::test]
    async fn admin_can_unsubscribe_foreign_subscription() {
        let svc = service_with_admin().await;
        let SubscriptionResult::Subscribed { summary: s, .. } = svc
            .handle(&uref("1"), subscribe_cmd(18 * 60, 20 * 60))
            .await
            .unwrap()
        else {
            panic!("expected Subscribed");
        };
        let reply = svc
            .handle(&admin_uref(), SubscriptionCommand::Unsubscribe { id: s.id })
            .await
            .unwrap();
        assert_eq!(reply, SubscriptionResult::Unsubscribed { id: s.id });
    }

    #[tokio::test]
    async fn non_admin_cannot_unsubscribe_foreign_subscription() {
        let svc = service_with_admin().await;
        let SubscriptionResult::Subscribed { summary: s, .. } = svc
            .handle(&uref("1"), subscribe_cmd(18 * 60, 20 * 60))
            .await
            .unwrap()
        else {
            panic!("expected Subscribed");
        };
        let reply = svc
            .handle(&uref("2"), SubscriptionCommand::Unsubscribe { id: s.id })
            .await
            .unwrap();
        assert_eq!(reply, SubscriptionResult::NotFound { id: s.id });
    }

    #[tokio::test]
    async fn subscribe_passes_courts_through_to_summary() {
        let svc = service().await;
        let reply = svc
            .handle(
                &uref("1"),
                SubscriptionCommand::Subscribe {
                    schedule: Schedule::Weekday(chrono::Weekday::Tue),
                    start_minute: 18 * 60,
                    end_minute: 20 * 60,
                    courts: Some(vec!["Court 2".into()]),
                    filter: None,
                },
            )
            .await
            .unwrap();
        let SubscriptionResult::Subscribed { summary, .. } = reply else {
            panic!("expected Subscribed");
        };
        assert_eq!(summary.courts, Some(vec!["Court 2".to_string()]));
    }

    #[tokio::test]
    async fn subscribe_canonicalizes_courts_by_number() {
        let svc = service().await;
        for input in ["court 2", "2", "Cort 2"] {
            let reply = svc
                .handle(
                    &uref("1"),
                    SubscriptionCommand::Subscribe {
                        schedule: Schedule::Weekday(chrono::Weekday::Tue),
                        start_minute: 18 * 60,
                        end_minute: 20 * 60,
                        courts: Some(vec![input.into()]),
                        filter: None,
                    },
                )
                .await
                .unwrap();
            let SubscriptionResult::Subscribed { summary, .. } = reply else {
                panic!("expected Subscribed for {input:?}");
            };
            assert_eq!(summary.courts, Some(vec!["Court 2".to_string()]));
        }
    }

    #[tokio::test]
    async fn subscribe_rejects_unknown_courts_and_stores_nothing() {
        let svc = service().await;
        let reply = svc
            .handle(
                &uref("1"),
                SubscriptionCommand::Subscribe {
                    schedule: Schedule::Weekday(chrono::Weekday::Tue),
                    start_minute: 18 * 60,
                    end_minute: 20 * 60,
                    courts: Some(vec!["Court 42".into(), "Court 5".into()]),
                    filter: None,
                },
            )
            .await
            .unwrap();
        let SubscriptionResult::UnknownCourts { unknown, available } = reply else {
            panic!("expected UnknownCourts, got {reply:?}");
        };
        assert_eq!(unknown, vec!["Court 42".to_string()]);
        assert!(available.contains(&"Court 2".to_string()));
        assert_eq!(
            svc.handle(&uref("1"), SubscriptionCommand::List)
                .await
                .unwrap(),
            SubscriptionResult::SubscriptionList(vec![])
        );
    }

    #[tokio::test]
    async fn subscribe_without_courts_takes_the_configured_default_filter() {
        let svc = service_defaulting_to_clay().await;
        let SubscriptionResult::Subscribed { summary, .. } = svc
            .handle(&uref("1"), subscribe_cmd(18 * 60, 20 * 60))
            .await
            .unwrap()
        else {
            panic!("expected Subscribed");
        };
        assert_eq!(summary.filter, CourtFilter::CLAY);
    }

    #[tokio::test]
    async fn naming_courts_overrides_the_default_filter() {
        let svc = service_defaulting_to_clay().await;
        let SubscriptionResult::Subscribed { summary, .. } = svc
            .handle(
                &uref("1"),
                SubscriptionCommand::Subscribe {
                    schedule: Schedule::Weekday(chrono::Weekday::Tue),
                    start_minute: 18 * 60,
                    end_minute: 20 * 60,
                    courts: Some(vec!["19".into()]),
                    filter: None,
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected Subscribed");
        };
        assert_eq!(summary.courts, Some(vec![SYNTHETIC_COURT.to_string()]));
        assert_eq!(summary.filter, CourtFilter::Any);
    }

    #[tokio::test]
    async fn an_explicit_filter_beats_the_configured_default() {
        let svc = service_defaulting_to_clay().await;
        let SubscriptionResult::Subscribed { summary, .. } = svc
            .handle(
                &uref("1"),
                SubscriptionCommand::Subscribe {
                    schedule: Schedule::Weekday(chrono::Weekday::Tue),
                    start_minute: 18 * 60,
                    end_minute: 20 * 60,
                    courts: None,
                    filter: Some(CourtFilter::Surface(CourtSurface::Synthetic)),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected Subscribed");
        };
        assert_eq!(
            summary.filter,
            CourtFilter::Surface(CourtSurface::Synthetic)
        );
    }

    #[tokio::test]
    async fn a_filter_contradicting_the_named_courts_is_rejected_and_stores_nothing() {
        let svc = service().await;
        let reply = svc
            .handle(
                &uref("1"),
                SubscriptionCommand::Subscribe {
                    schedule: Schedule::Weekday(chrono::Weekday::Tue),
                    start_minute: 18 * 60,
                    end_minute: 20 * 60,
                    courts: Some(vec!["19".into(), "2".into()]),
                    filter: Some(CourtFilter::CLAY),
                },
            )
            .await
            .unwrap();

        assert_eq!(
            reply,
            SubscriptionResult::FilterExcludesCourts {
                courts: vec![SYNTHETIC_COURT.to_string()],
                filter: CourtFilter::CLAY,
            }
        );
        assert_eq!(
            svc.handle(&uref("1"), SubscriptionCommand::List)
                .await
                .unwrap(),
            SubscriptionResult::SubscriptionList(vec![])
        );
    }

    #[tokio::test]
    async fn a_filter_matching_the_named_courts_is_accepted() {
        let svc = service().await;
        let SubscriptionResult::Subscribed { summary, .. } = svc
            .handle(
                &uref("1"),
                SubscriptionCommand::Subscribe {
                    schedule: Schedule::Weekday(chrono::Weekday::Tue),
                    start_minute: 18 * 60,
                    end_minute: 20 * 60,
                    courts: Some(vec!["2".into()]),
                    filter: Some(CourtFilter::CLAY),
                },
            )
            .await
            .unwrap()
        else {
            panic!("expected Subscribed");
        };
        assert_eq!(summary.filter, CourtFilter::CLAY);
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
            .handle(&uref("1"), subscribe_cmd(18 * 60, 20 * 60))
            .await
            .unwrap();

        let SubscriptionResult::Subscribed { open_slots, .. } = reply else {
            panic!("expected Subscribed, got {reply:?}");
        };
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
            .handle(
                &uref("1"),
                SubscriptionCommand::Subscribe {
                    schedule: Schedule::Weekday(chrono::Weekday::Tue),
                    start_minute: 18 * 60,
                    end_minute: 20 * 60,
                    courts: Some(vec!["Court 5".into()]),
                    filter: None,
                },
            )
            .await
            .unwrap();

        let SubscriptionResult::Subscribed { open_slots, .. } = reply else {
            panic!("expected Subscribed, got {reply:?}");
        };
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
            .handle(&uref("1"), subscribe_cmd(18 * 60, 20 * 60))
            .await
            .unwrap();

        let SubscriptionResult::Subscribed { open_slots, .. } = reply else {
            panic!("expected Subscribed, got {reply:?}");
        };
        assert_eq!(open_slots.len(), 1);
        assert_eq!(open_slots[0].court, "Court 5");
    }

    #[tokio::test]
    async fn subscribe_survives_slot_snapshot_failure_with_empty_preview() {
        let svc = service_with_failing_slot_snapshot().await;

        let reply = svc
            .handle(&uref("1"), subscribe_cmd(18 * 60, 20 * 60))
            .await
            .unwrap();

        let SubscriptionResult::Subscribed { open_slots, .. } = reply else {
            panic!("expected Subscribed, got {reply:?}");
        };
        assert!(open_slots.is_empty());

        let SubscriptionResult::SubscriptionList(subs) = svc
            .handle(&uref("1"), SubscriptionCommand::List)
            .await
            .unwrap()
        else {
            panic!("expected SubscriptionList");
        };
        assert_eq!(subs.len(), 1);
    }
}
