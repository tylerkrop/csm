use std::process::Command;

use anyhow::{bail, Context, Result};

/// Get the root directory of the current git repository.
pub fn repo_root() -> Result<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("Failed to run git")?;
    if !out.status.success() {
        bail!("Not inside a git repository");
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

/// Extract the repository name from its path.
pub fn repo_name(source_repo: &str) -> String {
    std::path::Path::new(source_repo)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Check if a git branch exists, optionally scoped to a specific repository.
pub fn branch_exists(branch: &str, source_repo: Option<&str>) -> bool {
    let mut cmd = Command::new("git");
    if let Some(repo) = source_repo {
        cmd.args(["-C", repo]);
    }
    cmd.args(["rev-parse", "--verify", branch])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Create a git worktree. Optionally scoped to a source repository
/// and optionally creating a new branch.
pub fn create_worktree(
    worktree_path: &str,
    branch: &str,
    new_branch: bool,
    source_repo: Option<&str>,
) -> Result<()> {
    if let Some(parent) = std::path::Path::new(worktree_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut cmd = Command::new("git");
    if let Some(repo) = source_repo {
        cmd.args(["-C", repo]);
    }
    cmd.arg("worktree").arg("add");
    if new_branch {
        cmd.args(["-b", branch, worktree_path]);
    } else {
        cmd.args([worktree_path, branch]);
    }

    let out = cmd.output().context("Failed to create worktree")?;
    if !out.status.success() {
        bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// Remove a worktree, falling back to directory removal + prune.
/// Prints a warning if removal fails entirely.
pub fn remove_worktree(source_repo: &str, worktree_path: &str) {
    let git_removed = Command::new("git")
        .args([
            "-C",
            source_repo,
            "worktree",
            "remove",
            worktree_path,
            "--force",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if git_removed {
        return;
    }

    // Fallback: remove directory manually
    if std::fs::remove_dir_all(worktree_path).is_err() {
        if std::path::Path::new(worktree_path).exists() {
            eprintln!("Warning: failed to remove worktree at {worktree_path}");
        }
    }
    let _ = Command::new("git")
        .args(["-C", source_repo, "worktree", "prune"])
        .output();
}

/// Read the current branch of a worktree directory. Returns None if the
/// directory doesn't exist or git fails.
pub fn current_branch(worktree_path: &str) -> Option<String> {
    // symbolic-ref works even on unborn branches (no commits yet)
    Command::new("git")
        .args(["-C", worktree_path, "symbolic-ref", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}
