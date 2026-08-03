use chrono::{DateTime, Utc};

use crate::time::{fmt_berlin, fmt_berlin_time};

pub(super) const DISCORD_CHUNK_BUDGET: usize = 1800;

pub(super) fn fmt_slot_line(court: &str, start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    format!(
        "{}: {}–{}",
        pad_court_number(court),
        fmt_berlin(start),
        fmt_berlin_time(end)
    )
}

pub(super) fn fmt_club_slot_line(
    club: &str,
    court: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> String {
    format!(
        "{} — {}: {}–{}",
        club,
        pad_court_number(court),
        fmt_berlin(start),
        fmt_berlin_time(end)
    )
}

fn pad_court_number(court: &str) -> String {
    let Some(start) = court.find(|ch: char| ch.is_ascii_digit()) else {
        return court.to_owned();
    };
    let end = court[start..]
        .find(|ch: char| !ch.is_ascii_digit())
        .map_or(court.len(), |end| start + end);
    format!(
        "{}{:0>2}{}",
        &court[..start],
        &court[start..end],
        &court[end..]
    )
}

pub(super) fn chunk_lines(lines: &[String], max_chars: usize) -> Vec<String> {
    assert!(max_chars > 0, "chunk size must be non-zero");
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_chars = 0;
    for line in lines {
        let line_chars = line.chars().count();
        if line_chars > max_chars {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                cur_chars = 0;
            }
            for ch in line.chars() {
                cur.push(ch);
                cur_chars += 1;
                if cur_chars == max_chars {
                    out.push(std::mem::take(&mut cur));
                    cur_chars = 0;
                }
            }
            continue;
        }

        let separator_len = if cur.is_empty() { 0 } else { 1 };
        if !cur.is_empty() && cur_chars + separator_len + line_chars > max_chars {
            out.push(std::mem::take(&mut cur));
            cur_chars = 0;
        }
        if !cur.is_empty() {
            cur.push('\n');
            cur_chars += 1;
        }
        cur.push_str(line);
        cur_chars += line_chars;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn slot_line_renders_berlin_local_times() {
        let start = Utc.with_ymd_and_hms(2026, 6, 2, 18, 0, 0).unwrap();
        let end = start + chrono::Duration::hours(1);
        assert_eq!(
            fmt_slot_line("Court 2", start, end),
            "Court 02: Tue, 02.06.2026 20:00–21:00"
        );
    }

    #[test]
    fn court_numbers_are_zero_padded_to_two_digits() {
        assert_eq!(pad_court_number("Court 2"), "Court 02");
        assert_eq!(pad_court_number("Court 25"), "Court 25");
        assert_eq!(pad_court_number("Court 6 (Outdoor)"), "Court 06 (Outdoor)");
        assert_eq!(pad_court_number("Centre Court"), "Centre Court");
    }

    #[test]
    fn chunking_counts_unicode_code_points_not_utf8_bytes() {
        let chunks = chunk_lines(&["äöü".to_string()], 3);
        assert_eq!(chunks, vec!["äöü"]);
    }

    #[test]
    fn chunking_splits_a_single_overlong_line() {
        let chunks = chunk_lines(&["abcdefgh".to_string()], 3);
        assert_eq!(chunks, vec!["abc", "def", "gh"]);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 3));
    }

    #[test]
    fn chunking_packs_lines_up_to_budget() {
        let lines = vec!["aa".to_string(), "bb".to_string(), "cc".to_string()];
        assert_eq!(chunk_lines(&lines, 5), vec!["aa\nbb", "cc"]);
    }
}
