use std::process::Command;

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
            .map(|out| {
                String::from_utf8_lossy(&out.stdout)
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
            })
            .unwrap_or_default();
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
/// Polls until the session disappears from the session list (100ms intervals, 5s timeout).
pub fn stop_and_cleanup(name: &str) {
    stop(name);
    for _ in 0..50 {
        cleanup(name);
        if !session_exists(name) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
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
