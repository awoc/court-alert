use super::text::{DISCORD_CHUNK_BUDGET, fmt_club_slot_line, fmt_slot_line};
use crate::model::{AlertLine, AvailabilityChange, BookableSlot};
use crate::subscriptions::contract::AvailabilityAlert;

const STRIKE_MARKUP_CHARS: usize = 4;

pub(super) fn render_line(line: &AlertLine) -> String {
    let text = match &line.club {
        Some(club) => fmt_club_slot_line(club, &line.court_name, line.starts_at, line.ends_at),
        None => fmt_slot_line(&line.court_name, line.starts_at, line.ends_at),
    };
    if line.struck {
        format!("~~{text}~~")
    } else {
        text
    }
}

pub(super) fn render(lines: &[AlertLine]) -> String {
    lines.iter().map(render_line).collect::<Vec<_>>().join("\n")
}

pub(super) fn alert_lines(alert: &AvailabilityAlert) -> Vec<AlertLine> {
    alert
        .slots
        .iter()
        .map(|slot| AlertLine {
            club: Some(slot.club.clone()),
            court_id: slot.court_id,
            court_name: slot.court.clone(),
            starts_at: slot.starts_at,
            ends_at: slot.ends_at,
            struck: false,
        })
        .collect()
}

pub(super) fn channel_lines(slots: &[&BookableSlot]) -> Vec<AlertLine> {
    slots.iter().copied().map(AlertLine::from).collect()
}

pub(super) fn chunk_lines(lines: Vec<AlertLine>) -> Vec<Vec<AlertLine>> {
    let mut chunks = Vec::new();
    let mut current: Vec<AlertLine> = Vec::new();
    let mut current_chars = 0;

    for line in lines {
        // Reserve room for adding strike markers in a later edit.
        let cost = render_line(&line).chars().count() + STRIKE_MARKUP_CHARS;
        let separator = usize::from(!current.is_empty());
        if !current.is_empty() && current_chars + separator + cost > DISCORD_CHUNK_BUDGET {
            chunks.push(std::mem::take(&mut current));
            current_chars = 0;
        }
        current_chars += usize::from(!current.is_empty()) + cost;
        current.push(line);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

pub(super) fn added_slots(changes: &[AvailabilityChange]) -> Vec<&BookableSlot> {
    let mut added = changes
        .iter()
        .filter_map(|change| match change {
            AvailabilityChange::BecameBookable(slot) => Some(slot),
            AvailabilityChange::BecameUnbookable(_) => None,
        })
        .collect::<Vec<_>>();
    added.sort_unstable_by(|left, right| {
        left.starts_at
            .cmp(&right.starts_at)
            .then_with(|| left.court_name.cmp(&right.court_name))
    });
    added
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscriptions::contract::AvailableSlotSummary;
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    const DISCORD_MESSAGE_LIMIT: usize = 2_000;

    fn slot(name: &str, hour: u32) -> BookableSlot {
        let starts_at = Utc.with_ymd_and_hms(2026, 6, 2, hour, 0, 0).unwrap();
        BookableSlot {
            venue_id: crate::model::VenueId::new("zhs-munich"),
            court_id: Uuid::new_v4(),
            court_name: name.into(),
            starts_at,
            ends_at: starts_at + chrono::Duration::hours(1),
            available_places: 1,
        }
    }

    fn strike(line: &AlertLine) -> AlertLine {
        AlertLine {
            struck: true,
            ..line.clone()
        }
    }

    fn channel_chunks(slots: &[&BookableSlot]) -> Vec<Vec<AlertLine>> {
        chunk_lines(channel_lines(slots))
    }

    fn alert(slots: Vec<AvailableSlotSummary>) -> AvailabilityAlert {
        AvailabilityAlert { slots }
    }

    fn dm_slot(club: &str, court: &str, hour: u32) -> AvailableSlotSummary {
        let announced = slot(court, hour);
        AvailableSlotSummary {
            club: club.into(),
            court: announced.court_name,
            court_id: announced.court_id,
            starts_at: announced.starts_at,
            ends_at: announced.ends_at,
        }
    }

    #[test]
    fn an_unstruck_message_has_no_header_and_one_line_per_slot() {
        let slots = [slot("Court 2", 18), slot("Court 5", 12)];
        let chunks = channel_chunks(&[&slots[0], &slots[1]]);

        let rendered = render(&chunks[0]);

        assert_eq!(
            rendered,
            "Court 02: Tue, 02.06.2026 20:00–21:00\nCourt 05: Tue, 02.06.2026 14:00–15:00"
        );
        assert!(!rendered.contains("Newly available"), "the header is gone");
    }

    #[test]
    fn a_struck_line_is_wrapped_individually() {
        let slots = [slot("Court 2", 18), slot("Court 5", 12)];
        let mut lines = channel_chunks(&[&slots[0], &slots[1]]).remove(0);
        lines[1] = strike(&lines[1]);

        let rendered = render(&lines);

        assert_eq!(
            rendered,
            "Court 02: Tue, 02.06.2026 20:00–21:00\n~~Court 05: Tue, 02.06.2026 14:00–15:00~~"
        );
    }

    #[test]
    fn every_struck_line_carries_its_own_markers() {
        let slots = [slot("Court 2", 18), slot("Court 5", 12)];
        let lines: Vec<_> = channel_chunks(&[&slots[0], &slots[1]])
            .remove(0)
            .iter()
            .map(strike)
            .collect();

        let rendered = render(&lines);

        assert_eq!(rendered.matches("~~").count(), 4, "two pairs, not one");
        for line in rendered.lines() {
            assert!(line.starts_with("~~") && line.ends_with("~~"));
        }
    }

    #[test]
    fn no_slots_produce_no_chunks() {
        assert!(channel_chunks(&[]).is_empty());
    }

    #[test]
    fn a_fully_struck_chunk_stays_within_discords_limit() {
        let slots: Vec<_> = (0..500)
            .map(|index| slot("Court 99", (index % 24) as u32))
            .collect();
        let borrowed: Vec<_> = slots.iter().collect();

        let chunks = channel_chunks(&borrowed);

        assert!(chunks.len() > 1, "the fixture must actually span chunks");
        for chunk in &chunks {
            let struck: Vec<_> = chunk.iter().map(strike).collect();
            assert!(
                render(&struck).chars().count() <= DISCORD_MESSAGE_LIMIT,
                "a fully struck chunk exceeded Discord's limit"
            );
        }
    }

    #[test]
    fn a_dm_line_names_the_club_and_keeps_the_court_it_can_be_struck_by() {
        let announced = dm_slot("ZHS München", "Court 2", 18);
        let lines = alert_lines(&alert(vec![announced.clone()]));

        assert_eq!(lines.len(), 1);
        assert_eq!(
            render(&lines),
            "ZHS München — Court 02: Tue, 02.06.2026 20:00–21:00"
        );
        assert_eq!(
            lines[0].court_id, announced.court_id,
            "without the court id the line could never be found again"
        );
    }

    #[test]
    fn a_struck_dm_line_wraps_the_club_too() {
        let lines: Vec<_> = alert_lines(&alert(vec![dm_slot("ZHS München", "Court 2", 18)]))
            .iter()
            .map(strike)
            .collect();

        assert_eq!(
            render(&lines),
            "~~ZHS München — Court 02: Tue, 02.06.2026 20:00–21:00~~"
        );
    }

    #[test]
    fn a_dm_is_chunked_with_room_for_its_strike_markers() {
        let slots = (0..500)
            .map(|index| dm_slot("ZHS München", "Court 99", (index % 24) as u32))
            .collect();

        let chunks = chunk_lines(alert_lines(&alert(slots)));

        assert!(chunks.len() > 1, "the fixture must actually span chunks");
        for chunk in &chunks {
            let struck: Vec<_> = chunk.iter().map(strike).collect();
            assert!(
                render(&struck).chars().count() <= DISCORD_MESSAGE_LIMIT,
                "a fully struck chunk exceeded Discord's limit"
            );
        }
    }

    #[test]
    fn dm_lines_keep_the_order_they_were_matched_in() {
        let lines = alert_lines(&alert(vec![
            dm_slot("ZHS München", "Court 2", 18),
            dm_slot("Casa Padel", "Court 5", 12),
        ]));

        let rendered = render(&lines);
        let rendered: Vec<&str> = rendered.lines().collect();

        assert!(rendered[0].starts_with("ZHS München — Court 02: "));
        assert!(rendered[1].starts_with("Casa Padel — Court 05: "));
    }

    #[test]
    fn additions_are_sorted_by_start_time() {
        let late = slot("Court 2", 18);
        let early = slot("Court 5", 12);
        let gone = slot("Court 7", 14);
        let changes = vec![
            AvailabilityChange::BecameBookable(late.clone()),
            AvailabilityChange::BecameUnbookable(gone.clone()),
            AvailabilityChange::BecameBookable(early.clone()),
        ];

        let added = added_slots(&changes);

        assert_eq!(
            added
                .iter()
                .map(|s| s.court_name.as_str())
                .collect::<Vec<_>>(),
            vec!["Court 5", "Court 2"],
            "sorted by start time"
        );
    }
}
