use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};

/// Build the zellij layout KDL. The `include_git` flag controls whether the
/// "git" tab (which runs `gitui`) is present: `gitui` fails outside a git
/// repository, so callers omit that tab for sessions started in a non-git
/// directory.
///
/// Defines up to three named tabs:
/// - "ai" — default shell, focused on launch so the copilot injector types
///   the resume command into this pane.
/// - "git" — runs `gitui` in the worktree (only when `include_git`).
/// - "edit" — runs `nvim` in the worktree.
fn layout_kdl(include_git: bool) -> String {
    let git_tab = if include_git {
        "    tab name=\"git\" {\n        pane command=\"gitui\"\n    }\n"
    } else {
        ""
    };
    format!(
        r#"layout {{
    default_tab_template {{
        pane size=1 borderless=true {{
            plugin location="tab-bar"
        }}
        children
        pane size=1 borderless=true {{
            plugin location="status-bar"
        }}
    }}
    tab name="ai" focus=true {{
        pane
    }}
{git_tab}    tab name="edit" {{
        pane command="nvim"
    }}
}}
"#
    )
}

/// Zellij configuration written to `~/.csm/config.kdl` and passed to every
/// freshly-launched session via `--config`. Uses the simplified (ASCII) UI
/// variant and removes the frame/border drawn around panes.
const CONFIG_KDL: &str = r#"simplified_ui true
pane_frames false
"#;

/// Write the csm zellij layout to `~/.csm/` (overwriting any existing file so
/// updates take effect on the next launch) and return its path so it can be
/// passed to `zellij -n`. `include_git` selects the git-tab variant; each
/// variant is written to a distinct file so concurrent launches with different
/// settings don't clobber each other's layout.
pub fn ensure_layout(include_git: bool) -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not determine home directory")?;
    let dir = home.join(".csm");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create {}", dir.display()))?;
    let file = if include_git { "layout.kdl" } else { "layout-nogit.kdl" };
    let path = dir.join(file);
    std::fs::write(&path, layout_kdl(include_git))
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(path)
}

/// Write the csm zellij config to `~/.csm/config.kdl` (overwriting any existing
/// file so updates to `CONFIG_KDL` take effect on the next launch) and return
/// its path so it can be passed to `zellij --config`.
pub fn ensure_config() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not determine home directory")?;
    let dir = home.join(".csm");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create {}", dir.display()))?;
    let path = dir.join("config.kdl");
    std::fs::write(&path, CONFIG_KDL)
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

    #[test]
    fn layout_includes_git_tab_conditionally() {
        let with_git = layout_kdl(true);
        assert!(with_git.contains("command=\"gitui\""));
        assert!(with_git.contains("name=\"git\""));
        assert!(with_git.contains("command=\"nvim\""));

        let without_git = layout_kdl(false);
        assert!(!without_git.contains("gitui"));
        assert!(!without_git.contains("name=\"git\""));
        assert!(without_git.contains("command=\"nvim\""));
        assert!(without_git.contains("name=\"ai\""));
    }
}
