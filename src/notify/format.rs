use crate::model::{AlertLine, AvailabilityChange, BookableSlot, BookableSlotId};
use crate::text::{DISCORD_CHUNK_BUDGET, fmt_slot_line};

/// Characters a strikethrough adds to a line: `~~` on each side. Chunking
/// budgets every line at its struck width so that a message which later has
/// every line struck still fits inside Discord's limit.
const STRIKE_MARKUP_CHARS: usize = 4;

/// Discord's `~~` does not span newlines, so each struck line carries its own
/// pair. Never wrap the whole message in one.
pub(super) fn render_line(line: &AlertLine) -> String {
    let text = fmt_slot_line(&line.court_name, line.starts_at, line.ends_at);
    if line.struck {
        format!("~~{text}~~")
    } else {
        text
    }
}

pub(super) fn render(lines: &[AlertLine]) -> String {
    lines.iter().map(render_line).collect::<Vec<_>>().join("\n")
}

/// Packs slots into messages, budgeting each line at its *struck* width so a
/// fully struck message still fits. Never splits a line, so one slot is always
/// one line is always one stored row.
pub(super) fn chunk_slots(slots: &[&BookableSlot]) -> Vec<Vec<AlertLine>> {
    let mut chunks = Vec::new();
    let mut current: Vec<AlertLine> = Vec::new();
    let mut current_chars = 0;

    for slot in slots {
        let line = AlertLine::from(*slot);
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

pub(super) fn removed_slot_ids(changes: &[AvailabilityChange]) -> Vec<BookableSlotId> {
    changes
        .iter()
        .filter_map(|change| match change {
            AvailabilityChange::BecameUnbookable(slot) => Some(BookableSlotId::from(slot)),
            AvailabilityChange::BecameBookable(_) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    const DISCORD_MESSAGE_LIMIT: usize = 2_000;

    fn slot(name: &str, hour: u32) -> BookableSlot {
        let starts_at = Utc.with_ymd_and_hms(2026, 6, 2, hour, 0, 0).unwrap();
        BookableSlot {
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

    #[test]
    fn an_unstruck_message_has_no_header_and_one_line_per_slot() {
        let slots = [slot("Court 2", 18), slot("Court 5", 12)];
        let chunks = chunk_slots(&[&slots[0], &slots[1]]);

        let rendered = render(&chunks[0]);

        assert_eq!(
            rendered,
            "• Court 2 : Tue, 02.06.2026 20:00–21:00\n• Court 5 : Tue, 02.06.2026 14:00–15:00"
        );
        assert!(!rendered.contains("Newly available"), "the header is gone");
    }

    #[test]
    fn a_struck_line_is_wrapped_individually() {
        let slots = [slot("Court 2", 18), slot("Court 5", 12)];
        let mut lines = chunk_slots(&[&slots[0], &slots[1]]).remove(0);
        lines[1] = strike(&lines[1]);

        let rendered = render(&lines);

        assert_eq!(
            rendered,
            "• Court 2 : Tue, 02.06.2026 20:00–21:00\n~~• Court 5 : Tue, 02.06.2026 14:00–15:00~~"
        );
    }

    #[test]
    fn every_struck_line_carries_its_own_markers() {
        let slots = [slot("Court 2", 18), slot("Court 5", 12)];
        let lines: Vec<_> = chunk_slots(&[&slots[0], &slots[1]])
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
        assert!(chunk_slots(&[]).is_empty());
    }

    /// The reason chunking budgets the struck width: an unstruck message packed
    /// to the budget would overflow Discord's limit once every line is struck.
    #[test]
    fn a_fully_struck_chunk_stays_within_discords_limit() {
        let slots: Vec<_> = (0..500)
            .map(|index| slot("Court 99", (index % 24) as u32))
            .collect();
        let borrowed: Vec<_> = slots.iter().collect();

        let chunks = chunk_slots(&borrowed);

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
    fn additions_are_sorted_and_removals_are_returned_as_ids() {
        let late = slot("Court 2", 18);
        let early = slot("Court 5", 12);
        let gone = slot("Court 7", 14);
        let changes = vec![
            AvailabilityChange::BecameBookable(late.clone()),
            AvailabilityChange::BecameUnbookable(gone.clone()),
            AvailabilityChange::BecameBookable(early.clone()),
        ];

        let added = added_slots(&changes);
        let removed = removed_slot_ids(&changes);

        assert_eq!(
            added
                .iter()
                .map(|s| s.court_name.as_str())
                .collect::<Vec<_>>(),
            vec!["Court 5", "Court 2"],
            "sorted by start time"
        );
        assert_eq!(removed, vec![BookableSlotId::from(&gone)]);
    }
}
