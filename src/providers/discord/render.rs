use crate::model::{Schedule, TimeRange};
use crate::subscriptions::contract::{AvailabilityAlert, SubscriptionResult, SubscriptionSummary};
use crate::text::{DISCORD_CHUNK_BUDGET, chunk_lines, fmt_slot_line};
use crate::time::fmt_hhmm;

pub(super) fn render_reply(reply: &SubscriptionResult) -> Vec<String> {
    let lines = match reply {
        SubscriptionResult::Subscribed {
            summary: s,
            open_slots,
        } => {
            let mut lines = vec![format!(
                "#{}: **{}** {} ({}). You'll get a direct message \
                 as soon as matching courts become free.",
                s.id,
                schedule_label(s.schedule),
                time_range_label(s.time_range),
                courts_label(s.courts.as_deref()),
            )];
            if !open_slots.is_empty() {
                lines.push(String::new());
                lines.push("**Currently free:**".to_string());
                lines.extend(
                    open_slots
                        .iter()
                        .map(|slot| fmt_slot_line(&slot.court, slot.starts_at, slot.ends_at)),
                );
            }
            lines
        }
        SubscriptionResult::SubscriptionList(subs) if subs.is_empty() => {
            vec!["No reminders yet. Create one with `/subscribe`.".to_string()]
        }
        SubscriptionResult::SubscriptionList(subs) => {
            let mut lines = vec!["**Your reminders:**".to_string()];
            lines.extend(subs.iter().map(summary_line));
            lines
        }
        SubscriptionResult::Unsubscribed { id } => vec![format!("Reminder #{id} deleted.")],
        SubscriptionResult::NotFound { id } => {
            vec![format!("No reminder #{id} found that belongs to you.")]
        }
        SubscriptionResult::InvalidTimeRange => vec!["'to' must be after 'from'.".to_string()],
        SubscriptionResult::InvalidSchedule => vec![
            "Invalid 'day': use a weekday (e.g. Thu) or a date that is not in the past."
                .to_string(),
        ],
        SubscriptionResult::UnknownCourts { unknown, available } => vec![
            format!("Unknown court(s): {}.", unknown.join(", ")),
            format!("Available courts: {}.", available.join(", ")),
        ],
        SubscriptionResult::AllSubscriptions(all) if all.is_empty() => {
            vec!["No reminders exist.".to_string()]
        }
        SubscriptionResult::AllSubscriptions(all) => {
            let mut lines = vec!["**All reminders:**".to_string()];
            lines.extend(all.iter().map(|a| {
                format!(
                    "{} – <@{}> ({})",
                    summary_line(&a.summary),
                    a.user.user_id,
                    a.user.provider
                )
            }));
            lines
        }
        SubscriptionResult::NotAuthorized => vec!["This command is for admins only.".to_string()],
    };
    chunk_lines(&lines, DISCORD_CHUNK_BUDGET)
}

pub(super) fn render_help() -> Vec<String> {
    let lines = [
        "**ZHS court reminders — commands:**",
        "`/subscribe day from to [courts]` — get a DM when a matching court becomes free",
        "• `day`: weekday for every week (e.g. `Thu`), or a date for one day \
         (e.g. `23.06.2026`; year optional)",
        "• `from`/`to`: time window as HH:MM, Berlin time (e.g. `18:00`, `20:00`)",
        "• `courts`: optional comma-separated court names (e.g. `Court 2, Court 5`); \
         omit for all courts",
        "`/list` — show your reminders",
        "`/unsubscribe id` — delete a reminder by its ID",
        "`/listall` — show all reminders (admin only)",
        "`/help` — show this overview",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();
    chunk_lines(&lines, DISCORD_CHUNK_BUDGET)
}

fn summary_line(s: &SubscriptionSummary) -> String {
    format!(
        "#{} – {} {} ({})",
        s.id,
        schedule_label(s.schedule),
        time_range_label(s.time_range),
        courts_label(s.courts.as_deref()),
    )
}

pub(super) fn render_alert(alert: &AvailabilityAlert) -> Vec<String> {
    let mut lines = vec!["**New free courts:**".to_string()];
    for s in &alert.slots {
        lines.push(fmt_slot_line(&s.court, s.starts_at, s.ends_at));
    }
    chunk_lines(&lines, DISCORD_CHUNK_BUDGET)
}

fn courts_label(courts: Option<&[String]>) -> String {
    courts
        .map(|v| v.join(", "))
        .unwrap_or_else(|| "all courts".to_string())
}

fn time_range_label(range: TimeRange) -> String {
    format!(
        "{}–{}",
        fmt_hhmm(range.start_minute()),
        fmt_hhmm(range.end_minute())
    )
}

fn schedule_label(schedule: Schedule) -> String {
    match schedule {
        Schedule::Weekday(w) => w.to_string(),
        Schedule::Date(d) => d.format("%d.%m.%Y").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProviderUserRef;
    use crate::subscriptions::contract::{AvailableSlotSummary, OwnedSubscriptionSummary};
    use chrono::{TimeZone, Utc, Weekday};

    fn summary() -> SubscriptionSummary {
        SubscriptionSummary {
            id: 7,
            schedule: Schedule::Weekday(Weekday::Tue),
            time_range: TimeRange::new(18 * 60, 20 * 60).unwrap(),
            courts: None,
        }
    }

    fn slot_info(court: &str) -> AvailableSlotSummary {
        let starts_at = Utc.with_ymd_and_hms(2026, 6, 2, 18, 0, 0).unwrap();
        AvailableSlotSummary {
            court: court.into(),
            starts_at,
            ends_at: starts_at + chrono::Duration::hours(1),
        }
    }

    fn reply_text(reply: &SubscriptionResult) -> String {
        render_reply(reply).join("\n")
    }

    fn subscribed(summary: SubscriptionSummary) -> SubscriptionResult {
        SubscriptionResult::Subscribed {
            summary,
            open_slots: vec![],
        }
    }

    #[test]
    fn renders_subscribed_confirmation() {
        let text = reply_text(&subscribed(summary()));
        assert!(text.contains("#7"));
        assert!(text.contains("Tue"));
        assert!(text.contains("18:00–20:00"));
        assert!(text.contains("all courts"));
        assert!(text.contains("direct message"));
    }

    #[test]
    fn renders_subscribed_with_court_filter() {
        let mut s = summary();
        s.courts = Some(vec!["Court 2".into(), "Court 5".into()]);
        let text = reply_text(&subscribed(s));
        assert!(text.contains("Court 2, Court 5"));
    }

    #[test]
    fn renders_subscribed_with_open_slot_preview() {
        let text = reply_text(&SubscriptionResult::Subscribed {
            summary: summary(),
            open_slots: vec![slot_info("Court 2")],
        });
        assert!(text.contains("**Currently free:**"));
        assert!(text.contains("• Court 2 : Tue, 02.06.2026 20:00–21:00"));
    }

    #[test]
    fn renders_subscribed_without_preview_section_when_no_open_slots() {
        let text = reply_text(&subscribed(summary()));
        assert!(!text.contains("Currently free"));
    }

    #[test]
    fn renders_empty_list_hint() {
        let text = reply_text(&SubscriptionResult::SubscriptionList(vec![]));
        assert!(text.contains("/subscribe"));
    }

    #[test]
    fn renders_list_lines() {
        let text = reply_text(&SubscriptionResult::SubscriptionList(vec![summary()]));
        assert!(text.contains("**Your reminders:**"));
        assert!(text.contains("#7 – Tue 18:00–20:00 (all courts)"));
    }

    #[test]
    fn renders_unsubscribe_outcomes() {
        assert_eq!(
            reply_text(&SubscriptionResult::Unsubscribed { id: 3 }),
            "Reminder #3 deleted."
        );
        assert!(reply_text(&SubscriptionResult::NotFound { id: 3 }).contains("#3"));
        assert!(reply_text(&SubscriptionResult::InvalidTimeRange).contains("'to'"));
    }

    #[test]
    fn renders_unknown_courts_with_available_alternatives() {
        let text = reply_text(&SubscriptionResult::UnknownCourts {
            unknown: vec!["Cort 2".into()],
            available: vec!["Court 2".into(), "Court 5".into()],
        });
        assert!(text.contains("Unknown court(s): Cort 2."));
        assert!(text.contains("Available courts: Court 2, Court 5."));
    }

    #[test]
    fn alert_renders_localized_slots_without_mention() {
        let msgs = render_alert(&AvailabilityAlert {
            slots: vec![slot_info("Court 2")],
        });
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].starts_with("**New free courts:**"));
        assert!(msgs[0].contains("• Court 2 : Tue, 02.06.2026 20:00–21:00"));
        assert!(!msgs[0].contains("<@"));
    }

    #[test]
    fn alert_chunks_under_discord_limit() {
        let slots: Vec<AvailableSlotSummary> = (0..500).map(|_| slot_info("Court 99")).collect();
        let msgs = render_alert(&AvailabilityAlert { slots });
        assert!(msgs.len() > 1);
        for m in &msgs {
            assert!(
                m.chars().count() <= DISCORD_CHUNK_BUDGET,
                "chunk too big: {}",
                m.chars().count()
            );
        }
    }

    #[test]
    fn alert_renders_multiple_slots_as_ordered_bullet_lines() {
        let msgs = render_alert(&AvailabilityAlert {
            slots: vec![slot_info("Court 2"), slot_info("Court 5")],
        });
        assert_eq!(msgs.len(), 1);
        let lines: Vec<&str> = msgs[0].lines().collect();
        assert_eq!(lines[0], "**New free courts:**");
        assert!(lines[1].starts_with("• Court 2 : "));
        assert!(lines[2].starts_with("• Court 5 : "));
    }

    #[test]
    fn renders_date_subscription_summary() {
        let mut s = summary();
        s.schedule = Schedule::Date(chrono::NaiveDate::from_ymd_opt(2026, 6, 23).unwrap());
        let text = reply_text(&subscribed(s));
        assert!(text.contains("23.06.2026"));
        assert!(!text.contains("Tue"));
    }

    #[test]
    fn renders_not_authorized() {
        let text = reply_text(&SubscriptionResult::NotAuthorized);
        assert!(text.to_lowercase().contains("admin"));
    }

    #[test]
    fn renders_listall_with_owner_and_schedule() {
        use super::super::PROVIDER_NAME;
        let reply = SubscriptionResult::AllSubscriptions(vec![OwnedSubscriptionSummary {
            user: ProviderUserRef {
                provider: PROVIDER_NAME.to_string(),
                user_id: "42".to_string(),
            },
            summary: summary(),
        }]);
        let text = reply_text(&reply);
        assert!(text.contains("42")); // owner id shown
        assert!(text.contains("#7")); // subscription id
        assert!(text.contains("Tue")); // schedule label
    }

    #[test]
    fn renders_empty_listall() {
        let text = reply_text(&SubscriptionResult::AllSubscriptions(vec![]));
        assert!(text.to_lowercase().contains("no reminders"));
    }

    #[test]
    fn help_explains_every_command() {
        let text = render_help().join("\n");
        for cmd in ["/subscribe", "/list", "/unsubscribe", "/listall", "/help"] {
            assert!(text.contains(cmd), "help is missing {cmd}");
        }
        assert!(text.contains("Thu"));
        assert!(text.contains("23.06.2026"));
    }

    #[test]
    fn subscription_lists_are_paginated_within_discord_limit() {
        let replies = render_reply(&SubscriptionResult::SubscriptionList(vec![summary(); 500]));
        assert!(replies.len() > 1);
        assert!(
            replies
                .iter()
                .all(|reply| reply.chars().count() <= DISCORD_CHUNK_BUDGET)
        );
    }

    #[test]
    fn a_single_overlong_court_filter_is_split_safely() {
        let mut s = summary();
        s.courts = Some(vec!["ä".repeat(DISCORD_CHUNK_BUDGET + 1)]);
        let replies = render_reply(&subscribed(s));
        assert!(replies.len() > 1);
        assert!(
            replies
                .iter()
                .all(|reply| reply.chars().count() <= DISCORD_CHUNK_BUDGET)
        );
    }
}
