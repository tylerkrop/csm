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
pub fn shortest_unique_prefixes(hex_ids: &[String]) -> Vec<usize> {
    hex_ids
        .iter()
        .map(|id| {
            for len in 1..=id.len() {
                let prefix = &id[..len];
                if hex_ids.iter().filter(|o| o.starts_with(prefix)).count() == 1 {
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
