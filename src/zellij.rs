use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// Build the zellij layout KDL for a session. The `ai` tab runs the csm copilot
/// launcher (`launcher`) as a command pane with the session `uuid` as its
/// argument, so zellij owns the copilot process just like it owns `gitui`/`nvim`
/// in the other tabs: when copilot exits, the pane shows the standard "press
/// Enter to re-run" prompt, and re-running resumes the session (the launcher
/// picks `--resume` after the first launch).
///
/// The `include_git` flag controls whether the "git" tab (which runs `gitui`)
/// is present: `gitui` fails outside a git repository, so callers omit that tab
/// for sessions started in a non-git directory.
///
/// Defines up to three named tabs:
/// - "ai" — runs the copilot launcher, focused on launch.
/// - "git" — runs `gitui` in the worktree (only when `include_git`).
/// - "edit" — runs `nvim` in the worktree.
fn layout_kdl(launcher: &str, uuid: &str, include_git: bool) -> String {
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
        pane command="{launcher}" {{
            args "{uuid}"
        }}
    }}
{git_tab}    tab name="edit" {{
        pane command="nvim"
    }}
}}
"#
    )
}

/// Shell launcher for the copilot pane, written to `~/.csm/launch-copilot.sh`
/// and invoked as `launch-copilot.sh <uuid>`.
///
/// It records a per-session marker under `~/.csm/markers/<uuid>` the first time
/// it runs and uses that marker to choose between creating (`--name`) and
/// resuming (`--resume`) the copilot session. This lets a static zellij command
/// pane be re-run (Enter) after copilot exits and resume the same conversation
/// instead of spawning a duplicate named session. The marker is written *before*
/// the first `--name` launch, so a session killed before copilot exits cleanly
/// still resumes (never duplicates) on its next launch.
const LAUNCHER_SCRIPT: &str = r#"#!/bin/sh
set -u
uuid="$1"
marker="${HOME}/.csm/markers/${uuid}"
if [ -f "$marker" ]; then
    exec copilot --yolo --autopilot --resume="$uuid"
fi
mkdir -p "${HOME}/.csm/markers"
: > "$marker"
exec copilot --yolo --autopilot --name="$uuid"
"#;

/// Re-parse a UUID before it is embedded in a layout file or marker path.
/// Defense in depth: these strings come from the database and are written into
/// files consumed by zellij and a shell script, so nothing that isn't a
/// well-formed UUID is ever allowed through.
fn validate_uuid(uuid: &str) -> Result<()> {
    uuid::Uuid::parse_str(uuid).with_context(|| format!("Invalid UUID: {uuid}"))?;
    Ok(())
}

/// Zellij configuration written to `~/.csm/config.kdl` and passed to every
/// freshly-launched session via `--config`. Uses the simplified (ASCII) UI
/// variant and removes the frame/border drawn around panes.
const CONFIG_KDL: &str = r#"simplified_ui true
pane_frames false
"#;

/// Write the per-session zellij layout to `~/.csm/layouts/<uuid>.kdl` and return
/// its path so it can be passed to `zellij -n`. Because the layout embeds the
/// session `uuid` (as the launcher argument), each session gets its own file
/// rather than a shared one: this keeps concurrent launches from clobbering
/// each other and lets the `ai` pane target the right copilot session.
/// `include_git` selects the git-tab variant.
pub fn ensure_layout(uuid: &str, launcher: &Path, include_git: bool) -> Result<PathBuf> {
    validate_uuid(uuid)?;
    let home = dirs::home_dir().context("Could not determine home directory")?;
    let dir = home.join(".csm").join("layouts");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create {}", dir.display()))?;
    let path = dir.join(format!("{uuid}.kdl"));
    let launcher = launcher.to_string_lossy();
    std::fs::write(&path, layout_kdl(&launcher, uuid, include_git))
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(path)
}

/// Write the copilot launcher script to `~/.csm/launch-copilot.sh` (overwriting
/// any existing copy so updates to `LAUNCHER_SCRIPT` take effect) with the
/// executable bit set, and return its path so it can be embedded in the layout
/// as the `ai` pane command.
pub fn ensure_launcher() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not determine home directory")?;
    let dir = home.join(".csm");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create {}", dir.display()))?;
    let path = dir.join("launch-copilot.sh");
    std::fs::write(&path, LAUNCHER_SCRIPT)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)
            .with_context(|| format!("Failed to stat {}", path.display()))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms)
            .with_context(|| format!("Failed to chmod {}", path.display()))?;
    }
    Ok(path)
}

/// Ensure the launcher marker for `uuid` exists so its next launch resumes
/// (`--resume`) rather than creates (`--name`) the copilot session. Called when
/// relaunching an existing session (`start`/`restore`), including sessions
/// created before the launcher existed, so they never spawn a duplicate named
/// session.
pub fn ensure_marker(uuid: &str) -> Result<()> {
    validate_uuid(uuid)?;
    let home = dirs::home_dir().context("Could not determine home directory")?;
    let dir = home.join(".csm").join("markers");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create {}", dir.display()))?;
    let path = dir.join(uuid);
    if !path.exists() {
        std::fs::File::create(&path)
            .with_context(|| format!("Failed to create {}", path.display()))?;
    }
    Ok(())
}

/// Best-effort removal of a session's launcher marker and per-session layout
/// file. Called when a session is permanently destroyed (`remove -f`); the uuid
/// is never reused, so these files would otherwise linger under `~/.csm`.
pub fn cleanup_session_files(uuid: &str) {
    if validate_uuid(uuid).is_err() {
        return;
    }
    if let Some(home) = dirs::home_dir() {
        let base = home.join(".csm");
        let _ = std::fs::remove_file(base.join("markers").join(uuid));
        let _ = std::fs::remove_file(base.join("layouts").join(format!("{uuid}.kdl")));
    }
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
        let launcher = "/home/u/.csm/launch-copilot.sh";
        let uuid = "11111111-1111-1111-1111-111111111111";
        let with_git = layout_kdl(launcher, uuid, true);
        assert!(with_git.contains("command=\"gitui\""));
        assert!(with_git.contains("name=\"git\""));
        assert!(with_git.contains("command=\"nvim\""));

        let without_git = layout_kdl(launcher, uuid, false);
        assert!(!without_git.contains("gitui"));
        assert!(!without_git.contains("name=\"git\""));
        assert!(without_git.contains("command=\"nvim\""));
        assert!(without_git.contains("name=\"ai\""));
    }

    #[test]
    fn ai_tab_runs_launcher_with_uuid() {
        let launcher = "/home/u/.csm/launch-copilot.sh";
        let uuid = "abcdef01-2345-6789-abcd-ef0123456789";
        let layout = layout_kdl(launcher, uuid, true);
        assert!(layout.contains(&format!("pane command=\"{launcher}\"")));
        assert!(layout.contains(&format!("args \"{uuid}\"")));
        assert!(layout.contains("name=\"ai\" focus=true"));
    }

    #[test]
    fn launcher_script_selects_name_then_resume() {
        assert!(LAUNCHER_SCRIPT.contains("--name=\"$uuid\""));
        assert!(LAUNCHER_SCRIPT.contains("--resume=\"$uuid\""));
        assert!(LAUNCHER_SCRIPT.contains("markers/${uuid}"));
    }

    #[test]
    fn validate_uuid_rejects_non_uuid() {
        for bad in [
            "",
            "----",
            "deadbeef",
            "; rm -rf / #",
            "12345678-1234-1234-1234-12345678",
            "not-a-uuid-at-all-really-no",
        ] {
            assert!(
                validate_uuid(bad).is_err(),
                "expected '{bad}' to be rejected"
            );
        }
        assert!(validate_uuid("abcdef01-2345-6789-abcd-ef0123456789").is_ok());
    }
}
