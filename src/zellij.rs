use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};

/// Layout written to `~/.csm/layout.kdl` and passed to every freshly-launched
/// zellij session. Defines three named tabs:
/// - "ai" — default shell, focused on launch so the copilot injector types
///   the resume command into this pane.
/// - "git" — runs `gitui` in the worktree.
/// - "edit" — runs `nvim` in the worktree.
const LAYOUT_KDL: &str = r#"layout {
    default_tab_template {
        pane size=1 borderless=true {
            plugin location="tab-bar"
        }
        children
        pane size=1 borderless=true {
            plugin location="status-bar"
        }
    }
    tab name="ai" focus=true {
        pane
    }
    tab name="git" {
        pane command="gitui"
    }
    tab name="edit" {
        pane command="nvim"
    }
}
"#;

/// Write the csm zellij layout to `~/.csm/layout.kdl` (overwriting any existing
/// file so updates to `LAYOUT_KDL` take effect on the next launch) and return
/// its path so it can be passed to `zellij --layout`.
pub fn ensure_layout() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not determine home directory")?;
    let dir = home.join(".csm");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create {}", dir.display()))?;
    let path = dir.join("layout.kdl");
    std::fs::write(&path, LAYOUT_KDL)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(path)
}

/// Query the current state of all zellij sessions.
pub struct State {
    sessions: Vec<(String, bool)>,
}

impl State {
    pub fn query() -> Self {
        let sessions = Command::new("zellij")
            .args(["list-sessions", "-n"])
            .output()
            .ok()
            .map(|out| parse_list_sessions(&String::from_utf8_lossy(&out.stdout)))
            .unwrap_or_default();
        Self { sessions }
    }

    #[cfg(test)]
    pub fn from_sessions(sessions: Vec<(String, bool)>) -> Self {
        Self { sessions }
    }

    pub fn is_running(&self, name: &str) -> bool {
        self.sessions.iter().any(|(n, r)| n == name && *r)
    }

    pub fn exists(&self, name: &str) -> bool {
        self.sessions.iter().any(|(n, _)| n == name)
    }

    /// Return a display-friendly status based on zellij state.
    pub fn display_status(&self, name: &str) -> &str {
        for (n, running) in &self.sessions {
            if n == name {
                return if *running { "running" } else { "exited" };
            }
        }
        "stopped"
    }
}

/// Kill a running zellij session.
pub fn stop(name: &str) {
    let _ = Command::new("zellij")
        .args(["kill-session", name])
        .output();
}

/// Delete a dead/exited zellij session.
pub fn cleanup(name: &str) {
    let _ = Command::new("zellij")
        .args(["delete-session", name])
        .output();
}

/// Kill a running zellij session and wait for it to be fully removed.
/// Polls until the session disappears from the session list (100ms intervals,
/// 5s timeout). Returns `true` if the session was confirmed removed,
/// `false` if the timeout elapsed first (zombie session).
#[must_use = "callers should check whether cleanup actually succeeded"]
pub fn stop_and_cleanup(name: &str) -> bool {
    stop(name);
    for _ in 0..50 {
        cleanup(name);
        if !session_exists(name) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

fn session_exists(name: &str) -> bool {
    Command::new("zellij")
        .args(["list-sessions", "-s"])
        .output()
        .ok()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|line| line.trim() == name)
        })
        .unwrap_or(false)
}

/// Wait for a zellij session to appear, then type a command into its first pane.
/// Spawns as a tokio task so it runs concurrently with the zellij client.
/// Returns a handle that can be aborted if zellij fails to start.
pub fn spawn_command_injector(session_name: String, command: String) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Poll until the session exists (100ms intervals, 30s timeout)
        for _ in 0..300 {
            if session_exists(&session_name) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        // Small delay for the shell to initialize
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let _ = Command::new("zellij")
            .args(["-s", &session_name, "action", "write-chars", &command])
            .output();
        let _ = Command::new("zellij")
            .args(["-s", &session_name, "action", "write", "10"])
            .output();
    })
}

/// Parse the output of `zellij list-sessions -n`. Each non-empty line begins
/// with a session name; the line is treated as "running" unless it contains
/// the literal `EXITED` marker. Exposed as a free function so it can be unit
/// tested without invoking the zellij binary.
fn parse_list_sessions(stdout: &str) -> Vec<(String, bool)> {
    stdout
        .lines()
        .filter_map(|line| {
            let name = line.split_whitespace().next()?;
            if name.is_empty() {
                return None;
            }
            let running = !line.contains("EXITED");
            Some((name.to_string(), running))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_output() {
        assert!(parse_list_sessions("").is_empty());
        assert!(parse_list_sessions("\n\n").is_empty());
    }

    #[test]
    fn parse_running_and_exited() {
        let out = "\
abc12345 [Created 2m ago]\n\
def67890 [Created 5m ago] (EXITED - attach to resume)\n\
fed09876 [Created 1h ago]\n";
        let parsed = parse_list_sessions(out);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0], ("abc12345".to_string(), true));
        assert_eq!(parsed[1], ("def67890".to_string(), false));
        assert_eq!(parsed[2], ("fed09876".to_string(), true));
    }

    #[test]
    fn parse_skips_blank_lines() {
        let out = "abc\n\n   \ndef\n";
        let parsed = parse_list_sessions(out);
        assert_eq!(parsed, vec![("abc".to_string(), true), ("def".to_string(), true)]);
    }

    #[test]
    fn state_helpers() {
        let s = State::from_sessions(vec![
            ("a".to_string(), true),
            ("b".to_string(), false),
        ]);
        assert!(s.is_running("a"));
        assert!(!s.is_running("b"));
        assert!(s.exists("a"));
        assert!(s.exists("b"));
        assert!(!s.exists("c"));
        assert_eq!(s.display_status("a"), "running");
        assert_eq!(s.display_status("b"), "exited");
        assert_eq!(s.display_status("c"), "stopped");
    }
}
