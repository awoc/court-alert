use crate::model::{CourtFilter, Schedule, TimeRange};
use crate::subscriptions::contract::{
    AvailabilityAlert, OwnedSubscriptionSummary, SubscriptionResult, SubscriptionSummary,
};
use crate::text::{DISCORD_CHUNK_BUDGET, chunk_lines, fmt_club_slot_line};
use crate::time::fmt_hhmm;

const MAX_UNSUBSCRIBE_BUTTONS: usize = 20;

pub(super) struct ReplyMessage {
    pub(super) content: String,
    pub(super) unsubscribe_id: Option<i64>,
}

pub(super) fn render_reply(reply: &SubscriptionResult) -> Vec<ReplyMessage> {
    let lines = match reply {
        SubscriptionResult::SubscriptionList(subs) if !subs.is_empty() => {
            return reminder_messages(
                "**Your reminders:**",
                subs.iter().map(|s| (summary_line(s), s.id)).collect(),
            );
        }
        SubscriptionResult::AllSubscriptions(all) if !all.is_empty() => {
            return reminder_messages(
                "**All reminders:**",
                all.iter()
                    .map(|a| (owned_summary_line(a), a.summary.id))
                    .collect(),
            );
        }
        SubscriptionResult::Subscribed {
            summary: s,
            open_slots,
        } => {
            let mut lines = vec![format!(
                "#{}: **{}** {} ({} at {}). You'll get a direct message \
                 as soon as matching courts become free.",
                s.id,
                schedule_label(s.schedule),
                time_range_label(s.time_range),
                scope_label(s.courts.as_deref(), s.filter),
                club_label(s),
            )];
            if !open_slots.is_empty() {
                lines.push(String::new());
                lines.push("**Currently free:**".to_string());
                lines.extend(open_slots.iter().map(|slot| {
                    fmt_club_slot_line(&slot.club, &slot.court, slot.starts_at, slot.ends_at)
                }));
            }
            lines
        }
        SubscriptionResult::SubscriptionList(_) => {
            vec!["No reminders yet. Create one with `/subscribe`.".to_string()]
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
        SubscriptionResult::FilterExcludesCourts { courts, filter } => vec![format!(
            "{} not {filter}, so `{filter}` would never match. \
             Drop the filter or pick other courts.",
            match courts.len() {
                1 => format!("{} is", courts[0]),
                _ => format!("{} are", courts.join(", ")),
            },
        )],
        SubscriptionResult::UnknownClub { unknown, available } => vec![
            format!("Unknown club: {unknown}."),
            format!("Available clubs: {}.", available.join(", ")),
        ],
        SubscriptionResult::NoClubsConfigured { sport } => {
            vec![format!(
                "No {sport} clubs are configured, so there is nothing to watch."
            )]
        }
        SubscriptionResult::AllSubscriptions(_) => {
            vec!["No reminders exist.".to_string()]
        }
        SubscriptionResult::NotAuthorized => vec!["This command is for admins only.".to_string()],
    };
    text_messages(lines)
}

pub(super) fn render_button_reply(reply: &SubscriptionResult) -> Vec<ReplyMessage> {
    match reply {
        SubscriptionResult::NotFound { id } => render_text(&format!(
            "Reminder #{id} is already gone. Run `/list` for the current ones."
        )),
        reply => render_reply(reply),
    }
}

pub(super) fn render_text(message: &str) -> Vec<ReplyMessage> {
    text_messages(vec![message.to_string()])
}

fn text_messages(lines: Vec<String>) -> Vec<ReplyMessage> {
    chunk_lines(&lines, DISCORD_CHUNK_BUDGET)
        .into_iter()
        .map(|content| ReplyMessage {
            content,
            unsubscribe_id: None,
        })
        .collect()
}

fn reminder_messages(header: &str, entries: Vec<(String, i64)>) -> Vec<ReplyMessage> {
    let mut messages = text_messages(vec![header.to_string()]);
    let (with_button, rest) = entries.split_at(entries.len().min(MAX_UNSUBSCRIBE_BUTTONS));
    for (line, id) in with_button {
        let mut chunks = text_messages(vec![line.clone()]);
        // An overlong line is split; the button belongs below its last chunk.
        if let Some(last) = chunks.last_mut() {
            last.unsubscribe_id = Some(*id);
        }
        messages.extend(chunks);
    }
    if !rest.is_empty() {
        let mut lines = vec![format!(
            "Only the first {MAX_UNSUBSCRIBE_BUTTONS} reminders come with a button; \
             delete the rest with `/unsubscribe id`:"
        )];
        lines.extend(rest.iter().map(|(line, _)| line.clone()));
        messages.extend(text_messages(lines));
    }
    messages
}

pub(super) fn render_help() -> Vec<ReplyMessage> {
    let lines = [
        "**Court reminders — commands:**",
        "`/subscribe day from to [courts] [surface]` — get a DM when a matching \
         **tennis** court becomes free",
        "• `day`: weekday for every week (e.g. `Thu`), or a date for one day \
         (e.g. `23.06.2026`; year optional)",
        "• `from`/`to`: time window as HH:MM, Berlin time (e.g. `18:00`, `20:00`)",
        "• `courts`: optional comma-separated court numbers (e.g. `2, 19`); \
         omit to watch a whole surface",
        "• `surface`: `clay`, `synthetic` or `all`; defaults to clay, or to the \
         courts you named",
        "`/padel day from to [club] [location]` — the same for **padel** courts",
        "• `club`: optional; omit to watch every configured padel club",
        "• `location`: `indoor`, `outdoor` or `any`; defaults to any",
        "`/list` — show your reminders, each with a button to delete it",
        "`/unsubscribe id` — delete a reminder by its ID",
        "`/listall` — show all reminders (admin only)",
        "`/help` — show this overview",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();
    text_messages(lines)
}

fn summary_line(s: &SubscriptionSummary) -> String {
    format!(
        "#{} – {} {} ({} at {})",
        s.id,
        schedule_label(s.schedule),
        time_range_label(s.time_range),
        scope_label(s.courts.as_deref(), s.filter),
        club_label(s),
    )
}

/// A subscription with no club watches every club of its sport, and that has
/// to read as such — a blank would be indistinguishable from one named club.
fn club_label(s: &SubscriptionSummary) -> String {
    match &s.club {
        Some(club) => club.clone(),
        None => format!("all {} clubs", s.sport),
    }
}

fn owned_summary_line(owned: &OwnedSubscriptionSummary) -> String {
    format!(
        "{} – <@{}> ({})",
        summary_line(&owned.summary),
        owned.user.user_id,
        owned.user.provider
    )
}

pub(super) fn render_alert(alert: &AvailabilityAlert) -> Vec<String> {
    let mut lines = vec!["**New free courts:**".to_string()];
    for s in &alert.slots {
        lines.push(fmt_club_slot_line(
            &s.club,
            &s.court,
            s.starts_at,
            s.ends_at,
        ));
    }
    chunk_lines(&lines, DISCORD_CHUNK_BUDGET)
}

fn scope_label(courts: Option<&[String]>, filter: CourtFilter) -> String {
    match (courts, filter) {
        (Some(courts), _) => courts.join(", "),
        (None, CourtFilter::Any) => "all courts".to_string(),
        (None, filter) => format!("all {filter} courts"),
    }
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
    use crate::model::{ProviderUserRef, Sport};
    use crate::subscriptions::contract::{AvailableSlotSummary, OwnedSubscriptionSummary};
    use chrono::{TimeZone, Utc, Weekday};

    fn summary() -> SubscriptionSummary {
        SubscriptionSummary {
            id: 7,
            sport: Sport::Tennis,
            club: None,
            schedule: Schedule::Weekday(Weekday::Tue),
            time_range: TimeRange::new(18 * 60, 20 * 60).unwrap(),
            courts: None,
            filter: CourtFilter::Any,
        }
    }

    fn slot_info(court: &str) -> AvailableSlotSummary {
        let starts_at = Utc.with_ymd_and_hms(2026, 6, 2, 18, 0, 0).unwrap();
        AvailableSlotSummary {
            club: "ZHS München".into(),
            court: court.into(),
            starts_at,
            ends_at: starts_at + chrono::Duration::hours(1),
        }
    }

    fn reply_text(reply: &SubscriptionResult) -> String {
        join(&render_reply(reply))
    }

    fn join(messages: &[ReplyMessage]) -> String {
        messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn buttons(messages: &[ReplyMessage]) -> Vec<i64> {
        messages.iter().filter_map(|m| m.unsubscribe_id).collect()
    }

    fn summaries(count: usize) -> Vec<SubscriptionSummary> {
        (0..count)
            .map(|i| SubscriptionSummary {
                id: i as i64 + 1,
                ..summary()
            })
            .collect()
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
        assert!(text.contains("• ZHS München — Court 2 : Tue, 02.06.2026 20:00–21:00"));
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
        assert!(text.contains("#7 – Tue 18:00–20:00 (all courts at all tennis clubs)"));
    }

    #[test]
    fn every_listed_reminder_gets_its_own_message_with_an_unsubscribe_button() {
        let messages = render_reply(&SubscriptionResult::SubscriptionList(summaries(3)));

        assert_eq!(messages.len(), 4); // header + one per reminder
        assert_eq!(messages[0].content, "**Your reminders:**");
        assert_eq!(messages[0].unsubscribe_id, None); // the header has no button
        for (message, id) in messages[1..].iter().zip(1..=3) {
            assert!(message.content.starts_with(&format!("#{id} – ")));
            assert_eq!(message.unsubscribe_id, Some(id));
        }
    }

    #[test]
    fn listed_reminders_beyond_the_button_limit_stay_plain_text() {
        let over_limit = MAX_UNSUBSCRIBE_BUTTONS + 2;
        let messages = render_reply(&SubscriptionResult::SubscriptionList(summaries(over_limit)));

        let ids: Vec<i64> = (1..=MAX_UNSUBSCRIBE_BUTTONS as i64).collect();
        assert_eq!(buttons(&messages), ids);
        let text = join(&messages);
        assert!(text.contains("/unsubscribe id"));
        for id in ids.len() + 1..=over_limit {
            assert!(
                text.contains(&format!("#{id} – ")),
                "reminder #{id} missing"
            );
        }
    }

    #[test]
    fn an_overlong_reminder_keeps_its_button_below_the_last_chunk() {
        let mut s = summary();
        s.courts = Some(vec!["ä".repeat(DISCORD_CHUNK_BUDGET + 1)]);
        let messages = render_reply(&SubscriptionResult::SubscriptionList(vec![s]));

        assert!(messages.len() > 2);
        assert_eq!(buttons(&messages), vec![7]);
        assert_eq!(messages.last().unwrap().unsubscribe_id, Some(7));
        assert!(
            messages
                .iter()
                .all(|m| m.content.chars().count() <= DISCORD_CHUNK_BUDGET)
        );
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
    fn a_button_that_finds_nothing_reports_a_reminder_already_gone() {
        let text = join(&render_button_reply(&SubscriptionResult::NotFound {
            id: 3,
        }));

        assert!(text.contains("#3"), "the reminder is not named: {text}");
        assert!(text.contains("already gone"), "reads as an error: {text}");
        // The slash command keeps its own wording, where a bad id is possible.
        assert!(reply_text(&SubscriptionResult::NotFound { id: 3 }).contains("belongs to you"));
    }

    #[test]
    fn a_button_renders_every_other_outcome_like_the_command_does() {
        for reply in [
            SubscriptionResult::Unsubscribed { id: 3 },
            SubscriptionResult::NotAuthorized,
            SubscriptionResult::SubscriptionList(summaries(3)),
        ] {
            assert_eq!(
                join(&render_button_reply(&reply)),
                join(&render_reply(&reply))
            );
        }
    }

    #[test]
    fn renders_the_surface_a_subscription_watches() {
        let mut s = summary();
        s.filter = CourtFilter::CLAY;
        assert!(reply_text(&subscribed(s.clone())).contains("all clay courts"));

        s.filter = CourtFilter::Surface(crate::model::CourtSurface::Synthetic);
        assert!(reply_text(&subscribed(s)).contains("all synthetic courts"));
    }

    #[test]
    fn named_courts_are_shown_instead_of_a_surface() {
        let mut s = summary();
        s.courts = Some(vec!["Court 19 - Synthetic".into()]);
        s.filter = CourtFilter::Any;
        let text = reply_text(&subscribed(s));
        assert!(text.contains("Court 19 - Synthetic"));
        assert!(!text.contains("all courts"));
    }

    #[test]
    fn renders_a_surface_that_excludes_the_named_courts() {
        let text = reply_text(&SubscriptionResult::FilterExcludesCourts {
            courts: vec!["Court 19 - Synthetic".into()],
            filter: CourtFilter::CLAY,
        });
        assert!(text.contains("Court 19 - Synthetic is not clay"));
        assert!(text.contains("`clay`"));
    }

    /// A blank would be indistinguishable from a named club, so "all clubs"
    /// has to be spelled out — and it names the sport, since that is what
    /// bounds it.
    #[test]
    fn a_subscription_without_a_club_reads_as_all_clubs_of_its_sport() {
        let mut s = summary();
        s.sport = Sport::Padel;
        s.club = None;
        assert!(reply_text(&subscribed(s.clone())).contains("all padel clubs"));

        s.club = Some("Casa Padel".into());
        let text = reply_text(&subscribed(s));
        assert!(text.contains("Casa Padel"));
        assert!(!text.contains("all padel clubs"));
    }

    #[test]
    fn renders_an_unknown_club_with_available_alternatives() {
        let text = reply_text(&SubscriptionResult::UnknownClub {
            unknown: "not-a-club".into(),
            available: vec!["Casa Padel".into()],
        });
        assert!(text.contains("Unknown club: not-a-club."));
        assert!(text.contains("Available clubs: Casa Padel."));
    }

    #[test]
    fn renders_a_sport_with_no_configured_clubs() {
        let text = reply_text(&SubscriptionResult::NoClubsConfigured {
            sport: Sport::Padel,
        });
        assert!(text.contains("No padel clubs"), "got: {text}");
    }

    #[test]
    fn help_covers_padel_as_well_as_subscribe() {
        let text = join(&render_help());
        for cmd in ["/subscribe", "/padel", "/list", "/unsubscribe", "/listall"] {
            assert!(text.contains(cmd), "help is missing {cmd}");
        }
        assert!(text.contains("indoor"));
        assert!(text.contains("club"));
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
        assert!(msgs[0].contains("• ZHS München — Court 2 : Tue, 02.06.2026 20:00–21:00"));
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
        assert!(lines[1].starts_with("• ZHS München — Court 2 : "));
        assert!(lines[2].starts_with("• ZHS München — Court 5 : "));
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
        let messages = render_reply(&reply);
        let text = join(&messages);
        assert!(text.contains("42")); // owner id shown
        assert!(text.contains("#7")); // subscription id
        assert!(text.contains("Tue")); // schedule label
        assert_eq!(buttons(&messages), vec![7]); // admins unsubscribe by button too
    }

    #[test]
    fn renders_empty_listall() {
        let text = reply_text(&SubscriptionResult::AllSubscriptions(vec![]));
        assert!(text.to_lowercase().contains("no reminders"));
    }

    #[test]
    fn help_explains_every_command() {
        let text = join(&render_help());
        for cmd in ["/subscribe", "/list", "/unsubscribe", "/listall", "/help"] {
            assert!(text.contains(cmd), "help is missing {cmd}");
        }
        assert!(text.contains("Thu"));
        assert!(text.contains("23.06.2026"));
    }

    #[test]
    fn subscription_lists_are_paginated_within_discord_limit() {
        let replies = render_reply(&SubscriptionResult::SubscriptionList(summaries(500)));
        assert!(replies.len() > 1);
        assert!(
            replies
                .iter()
                .all(|reply| reply.content.chars().count() <= DISCORD_CHUNK_BUDGET)
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
                .all(|reply| reply.content.chars().count() <= DISCORD_CHUNK_BUDGET)
        );
    }
}
