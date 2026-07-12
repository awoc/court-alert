use crate::domain::{AvailabilityChange, BookableSlot};
use crate::text::{DISCORD_CHUNK_BUDGET, chunk_lines, fmt_slot_line};

fn push_section(lines: &mut Vec<String>, header: &str, slots: &[&BookableSlot]) {
    if slots.is_empty() {
        return;
    }
    if !lines.is_empty() {
        lines.push(String::new());
    }
    lines.push(header.to_string());
    for slot in slots {
        lines.push(fmt_slot_line(
            &slot.court_name,
            slot.starts_at,
            slot.ends_at,
        ));
    }
}

pub(super) fn format_messages(changes: &[AvailabilityChange]) -> Vec<String> {
    let mut additions = changes
        .iter()
        .filter_map(|change| match change {
            AvailabilityChange::BecameBookable(slot) => Some(slot),
            AvailabilityChange::BecameUnbookable(_) => None,
        })
        .collect::<Vec<_>>();
    let mut removals = changes
        .iter()
        .filter_map(|change| match change {
            AvailabilityChange::BecameUnbookable(slot) => Some(slot),
            AvailabilityChange::BecameBookable(_) => None,
        })
        .collect::<Vec<_>>();
    let order = |left: &&BookableSlot, right: &&BookableSlot| {
        left.starts_at
            .cmp(&right.starts_at)
            .then_with(|| left.court_name.cmp(&right.court_name))
    };
    additions.sort_unstable_by(order);
    removals.sort_unstable_by(order);

    let mut lines = Vec::new();
    push_section(&mut lines, "**Newly available:**", &additions);
    push_section(&mut lines, "**No longer available:**", &removals);
    chunk_lines(&lines, DISCORD_CHUNK_BUDGET)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    fn slot(name: &str, hour: u32) -> BookableSlot {
        let starts_at = Utc.with_ymd_and_hms(2026, 6, 2, hour, 0, 0).unwrap();
        BookableSlot {
            court_id: Uuid::nil(),
            court_name: name.into(),
            starts_at,
            ends_at: starts_at + chrono::Duration::hours(1),
            available_places: 1,
        }
    }

    #[test]
    fn formats_additions_and_removals() {
        let changes = vec![
            AvailabilityChange::BecameBookable(slot("Court 2", 18)),
            AvailabilityChange::BecameUnbookable(slot("Court 5", 14)),
        ];
        let messages = format_messages(&changes);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("**Newly available:**"));
        assert!(messages[0].contains("• Court 2 : Tue, 02.06.2026 20:00–21:00"));
        assert!(messages[0].contains("**No longer available:**"));
    }

    #[test]
    fn empty_changes_produce_no_messages() {
        assert!(format_messages(&[]).is_empty());
    }

    #[test]
    fn chunks_long_snapshots_under_discord_limit() {
        let changes = (0..500)
            .map(|index| AvailabilityChange::BecameBookable(slot("Court 99", (index % 24) as u32)))
            .collect::<Vec<_>>();
        let messages = format_messages(&changes);
        assert!(messages.len() > 1);
        assert!(
            messages
                .iter()
                .all(|message| message.chars().count() <= DISCORD_CHUNK_BUDGET)
        );
    }
}
