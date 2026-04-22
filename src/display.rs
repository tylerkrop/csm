use std::io::IsTerminal;

use chrono::{NaiveDateTime, Utc};

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const MAGENTA: &str = "\x1b[35m";
const CYAN: &str = "\x1b[36m";

const SHORTCODE_DISPLAY_LEN: usize = 8;

/// Return the short (8-char hex) prefix of a UUID.
pub fn short_uuid(uuid: &str) -> String {
    let hex = uuid_hex(uuid);
    hex[..SHORTCODE_DISPLAY_LEN.min(hex.len())].to_string()
}

pub fn use_color() -> bool {
    std::io::stdout().is_terminal()
}

/// Format a UTC timestamp as a short relative time string (e.g., "3m", "2h", "5d").
pub fn relative_time(timestamp: &str) -> String {
    let Ok(then) = NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%d %H:%M:%S") else {
        return timestamp.to_string();
    };
    let secs = Utc::now().naive_utc().signed_duration_since(then).num_seconds();
    if secs < 60 {
        "now".to_string()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else if secs < 604800 {
        format!("{}d", secs / 86400)
    } else if secs < 2_592_000 {
        format!("{}w", secs / 604800)
    } else {
        format!("{}mo", secs / 2_592_000)
    }
}

/// Strip hyphens from a UUID for shortcode display.
pub fn uuid_hex(uuid: &str) -> String {
    uuid.replace('-', "")
}

/// Compute the shortest unique prefix length for each hex ID in a list.
#[cfg(test)]
pub fn shortest_unique_prefixes(hex_ids: &[String]) -> Vec<usize> {
    shortest_unique_prefixes_within(hex_ids, hex_ids)
}

/// Compute the shortest unique prefix length for each hex ID in `hex_ids`,
/// where uniqueness is measured against the full `universe` of hex IDs (which
/// must include every entry in `hex_ids`). This lets callers display only a
/// subset of sessions while still producing prefixes that unambiguously
/// identify the entry across all sessions in storage.
pub fn shortest_unique_prefixes_within(hex_ids: &[String], universe: &[String]) -> Vec<usize> {
    hex_ids
        .iter()
        .map(|id| {
            for len in 1..=id.len() {
                let prefix = &id[..len];
                if universe.iter().filter(|o| o.starts_with(prefix)).count() == 1 {
                    return len;
                }
            }
            id.len()
        })
        .collect()
}

/// Format a shortcode with jj-style coloring: the unique prefix is bold,
/// the remainder is dimmed.
pub fn format_shortcode(hex: &str, unique_len: usize, color: bool) -> String {
    let display_len = SHORTCODE_DISPLAY_LEN.min(hex.len());
    let cut = unique_len.min(display_len);
    let (unique, rest) = (&hex[..cut], &hex[cut..display_len]);
    if color {
        format!("{BOLD}{MAGENTA}{unique}{RESET}{DIM}{MAGENTA}{rest}{RESET}")
    } else {
        format!("{unique}{rest}")
    }
}

/// Format a session line for `ps` output.
pub fn format_session_line(
    shortcode: &str,
    name: &str,
    repo: &str,
    branch: &str,
    status: &str,
    last_used: &str,
    color: bool,
) -> String {
    let ago = relative_time(last_used);
    if color {
        let name = format!("{BOLD}{name}{RESET}");
        let repo = format!("{CYAN}{repo}{RESET}");
        let branch = format!("{MAGENTA}{branch}{RESET}");
        let status = match status {
            "running" => format!("{GREEN}{status}{RESET}"),
            "exited" => format!("{YELLOW}{status}{RESET}"),
            _ => format!("{DIM}{status}{RESET}"),
        };
        let ago = format!("{DIM}[{ago}]{RESET}");
        format!("{shortcode} {name} {repo} {branch} {status} {ago}")
    } else {
        format!("{shortcode} {name} {repo} {branch} {status} [{ago}]")
    }
}

/// Rank a status string for sorting (lower = higher priority).
pub fn status_rank(s: &str) -> u8 {
    match s {
        "running" => 0,
        "exited" => 1,
        "stopped" => 2,
        "removed" => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn uuid_hex_strips_hyphens() {
        assert_eq!(
            uuid_hex("12345678-1234-1234-1234-123456789abc"),
            "1234567812341234123412345678 9abc".replace(' ', "")
        );
        assert_eq!(uuid_hex(""), "");
        assert_eq!(uuid_hex("nodashes"), "nodashes");
    }

    #[test]
    fn short_uuid_returns_8_hex_chars() {
        assert_eq!(short_uuid("abcdef01-2345-6789-abcd-ef0123456789"), "abcdef01");
        // Shorter input shouldn't panic
        assert_eq!(short_uuid("abc"), "abc");
        assert_eq!(short_uuid(""), "");
    }

    #[test]
    fn shortest_unique_prefixes_basic() {
        let ids: Vec<String> = ["abcd", "abef", "wxyz"].iter().map(|s| s.to_string()).collect();
        assert_eq!(shortest_unique_prefixes(&ids), vec![3, 3, 1]);
    }

    #[test]
    fn shortest_unique_prefixes_single() {
        let ids = vec!["abcd".to_string()];
        assert_eq!(shortest_unique_prefixes(&ids), vec![1]);
    }

    #[test]
    fn shortest_unique_prefixes_empty_list() {
        let ids: Vec<String> = vec![];
        assert!(shortest_unique_prefixes(&ids).is_empty());
    }

    #[test]
    fn shortest_unique_prefixes_identical_returns_full_len() {
        let ids = vec!["abcd".to_string(), "abcd".to_string()];
        // No prefix is unique; falls back to full length.
        assert_eq!(shortest_unique_prefixes(&ids), vec![4, 4]);
    }

    #[test]
    fn shortest_unique_prefixes_one_is_prefix_of_other() {
        let ids = vec!["abc".to_string(), "abcd".to_string()];
        // "abc" is never unique on its own (the other starts with it),
        // so it falls back to full length. "abcd" is unique at len 4.
        assert_eq!(shortest_unique_prefixes(&ids), vec![3, 4]);
    }

    #[test]
    fn shortest_unique_prefixes_within_uses_full_universe() {
        // Visible IDs look unambiguous at len 1 ("a" vs "w"), but the hidden
        // entry "abff" in the universe forces "abcd" to need a longer prefix.
        let visible = vec!["abcd".to_string(), "wxyz".to_string()];
        let universe = vec![
            "abcd".to_string(),
            "abff".to_string(),
            "wxyz".to_string(),
        ];
        assert_eq!(shortest_unique_prefixes_within(&visible, &universe), vec![3, 1]);
    }

    #[test]
    fn relative_time_buckets() {
        let fmt = "%Y-%m-%d %H:%M:%S";
        let now = Utc::now().naive_utc();
        let cases = [
            (Duration::seconds(10), "now"),
            (Duration::seconds(120), "2m"),
            (Duration::hours(3), "3h"),
            (Duration::days(2), "2d"),
            (Duration::days(14), "2w"),
            (Duration::days(60), "2mo"),
        ];
        for (delta, expected) in cases {
            let ts = (now - delta).format(fmt).to_string();
            assert_eq!(relative_time(&ts), expected, "delta={delta:?}");
        }
    }

    #[test]
    fn relative_time_invalid_returns_input() {
        assert_eq!(relative_time("not a timestamp"), "not a timestamp");
    }

    #[test]
    fn status_rank_ordering() {
        assert!(status_rank("running") < status_rank("exited"));
        assert!(status_rank("exited") < status_rank("stopped"));
        assert!(status_rank("stopped") < status_rank("removed"));
        assert!(status_rank("removed") < status_rank("anything else"));
    }

    #[test]
    fn format_shortcode_no_color_truncates_to_8() {
        let hex = "abcdef0123456789";
        let s = format_shortcode(hex, 3, false);
        assert_eq!(s, "abcdef01");
    }

    #[test]
    fn format_shortcode_clamps_unique_len() {
        let hex = "abcdef0123456789";
        // unique_len greater than display window should not panic.
        let s = format_shortcode(hex, 99, false);
        assert_eq!(s, "abcdef01");
    }

    #[test]
    fn format_shortcode_color_contains_ansi() {
        let s = format_shortcode("abcdef0123", 3, true);
        assert!(s.contains("\x1b["));
        assert!(s.contains("abc"));
        assert!(s.contains("def01"));
    }

    #[test]
    fn format_session_line_no_color_layout() {
        let line = format_session_line(
            "abcd1234",
            "myname",
            "myrepo",
            "tk/branch",
            "running",
            "1970-01-01 00:00:00",
            false,
        );
        assert!(line.starts_with("abcd1234 myname myrepo tk/branch running ["));
        assert!(line.ends_with(']'));
        // No ANSI escapes when color is off.
        assert!(!line.contains("\x1b["));
    }
}
