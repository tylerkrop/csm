use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
    QueryOrder, TransactionTrait,
};
use uuid::Uuid;

use crate::display;
use crate::entity::session::{self, ActiveModel, Column, Entity as Session};
use crate::git;
use crate::interactive;
use crate::zellij;

// ── Constants ───────────────────────────────────────────────────────────────

const STATUS_ACTIVE: &str = "active";
const STATUS_REMOVED: &str = "removed";
const BRANCH_PREFIX: &str = "tylerkrop";

// ── Shared helpers ──────────────────────────────────────────────────────────

fn csm_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not determine home directory")?;
    Ok(home.join(".csm"))
}

fn now_str() -> String {
    Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Whole days elapsed since a stored `last_used_at` timestamp. Returns None if
/// the timestamp can't be parsed.
fn days_since(timestamp: &str) -> Option<i64> {
    let then = chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%d %H:%M:%S").ok()?;
    let secs = Utc::now()
        .naive_utc()
        .signed_duration_since(then)
        .num_seconds();
    Some(secs / 86_400)
}

/// The zellij session name is the 8-char hex prefix of the copilot UUID.
fn zellij_session_name(session: &session::Model) -> String {
    display::short_uuid(&session.copilot_uuid)
}

/// Prompt the user for a yes/no answer on stderr, reading a line from stdin.
/// Returns `true` only for an explicit yes; the default (empty input, EOF, or
/// a non-tty where the read fails) is `false`.
fn confirm(prompt: &str) -> bool {
    use std::io::{self, Write};
    eprint!("{prompt} [y/N] ");
    let _ = io::stderr().flush();
    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Find an unused session name derived from `base`. Returns `base` unchanged if
/// no session row currently uses it, otherwise appends `-2`, `-3`, … until a
/// free name is found. This lets the same branch name be reused across
/// repositories without a hard error, since the DB primary key is the human
/// session name.
async fn next_available_name(db: &DatabaseConnection, base: &str) -> Result<String> {
    if Session::find_by_id(base).one(db).await?.is_none() {
        return Ok(base.to_string());
    }
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if Session::find_by_id(&candidate).one(db).await?.is_none() {
            return Ok(candidate);
        }
    }
    unreachable!("integer range is effectively unbounded")
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Session name cannot be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        bail!("Session name must only contain alphanumeric characters, hyphens, or underscores");
    }
    Ok(())
}

/// Resolve a session by exact name or UUID shortcode prefix.
async fn resolve_session(db: &DatabaseConnection, query: &str) -> Result<session::Model> {
    if let Some(s) = Session::find_by_id(query).one(db).await? {
        return Ok(s);
    }

    let all = Session::find().all(db).await?;
    let matches: Vec<_> = all
        .into_iter()
        .filter(|s| display::uuid_hex(&s.copilot_uuid).starts_with(query))
        .collect();

    match matches.len() {
        0 => bail!("No session found matching '{query}'"),
        1 => Ok(matches.into_iter().next().unwrap()),
        _ => {
            let names: Vec<String> = matches.iter().map(|s| s.name.clone()).collect();
            bail!(
                "Ambiguous identifier '{query}' matches {} sessions: {}. Use a longer prefix.",
                names.len(),
                names.join(", ")
            );
        }
    }
}

/// Run the zellij client, then update last_used_at when the user detaches.
/// If the user quit zellij (Ctrl+q), cleans up the exited session so it
/// shows as "stopped" rather than "exited" in `csm ls`.
async fn enter_zellij(
    db: &DatabaseConnection,
    session_name: &str,
    zellij_name: &str,
    mut cmd: Command,
) -> Result<()> {
    cmd.status().context("Failed to run zellij")?;

    // User detached or quit — update last_used_at
    if let Some(session) = Session::find_by_id(session_name).one(db).await? {
        let mut active: ActiveModel = session.into();
        active.last_used_at = Set(now_str());
        active.update(db).await?;
    } else {
        eprintln!(
            "Warning: session '{session_name}' missing from database after detach; \
             zellij session '{zellij_name}' may be orphaned."
        );
    }

    // If the user quit (Ctrl+q), the session is EXITED but not removed.
    // Clean it up so `csm ls` shows "stopped" instead of "exited".
    let zs = zellij::State::query();
    if zs.exists(zellij_name) && !zs.is_running(zellij_name) {
        zellij::cleanup(zellij_name);
    }

    Ok(())
}

/// Launch a fresh zellij session whose `ai` tab runs the copilot launcher.
/// Used by `run`, `start`, and `restore`, which share the same startup shape.
/// The launcher itself picks `copilot --name` (first launch) vs
/// `copilot --resume` (subsequent launches) via a per-session marker, so csm no
/// longer types the command into the pane. Pass `resume = true` when relaunching
/// an existing session (`start`/`restore`) so the marker is ensured up front and
/// the launcher resumes even for sessions created before the launcher existed;
/// pass `resume = false` for a brand-new session (`run`), letting the launcher
/// create the marker on its first `--name` launch.
async fn start_zellij_session(
    db: &DatabaseConnection,
    session_name: &str,
    zellij_name: &str,
    uuid: &str,
    worktree: &str,
    resume: bool,
    include_git: bool,
) -> Result<()> {
    let launcher = zellij::ensure_launcher()?;
    let layout = zellij::ensure_layout(uuid, &launcher, include_git)?;
    let config = zellij::ensure_config()?;
    if resume {
        zellij::ensure_marker(uuid)?;
    }
    let mut cmd = Command::new("zellij");
    // `-n` (--new-session-with-layout) always creates a new session from the
    // given layout file, even when the caller is already inside zellij. We
    // still pass `-s` to set the session name. `--layout` would instead try
    // to attach to an existing session and add the layout as new tabs.
    cmd.arg("--config")
        .arg(&config)
        .args(["-s", zellij_name, "-n"])
        .arg(&layout)
        .current_dir(worktree);
    enter_zellij(db, session_name, zellij_name, cmd).await
}

// ── Commands ────────────────────────────────────────────────────────────────

pub async fn run(name: &str, here: bool) -> Result<()> {
    validate_name(name)?;
    let db = crate::db::connect().await?;
    let dir = csm_dir()?;

    // Resolve the DB session name (primary key). A removed session's name is
    // reclaimed; a live collision is disambiguated with a numeric suffix so the
    // same branch name can be reused across repositories without an error. The
    // branch itself always derives from the requested name, not the suffixed
    // session name.
    let session_name = match Session::find_by_id(name).one(&db).await? {
        Some(existing) if existing.status == STATUS_ACTIVE => {
            let unique = next_available_name(&db, name).await?;
            eprintln!("Session name '{name}' is already in use; using '{unique}' instead.");
            unique
        }
        Some(_) => {
            session::Entity::delete_by_id(name.to_string())
                .exec(&db)
                .await?;
            name.to_string()
        }
        None => name.to_string(),
    };

    let uuid = Uuid::new_v4().to_string();
    let zellij_name = display::short_uuid(&uuid);

    // Determine where copilot runs. Three cases:
    // - `--here`: run directly in the current directory (no branch/worktree),
    //   even inside a git repo. Useful for hobby projects.
    // - inside a git repo: create a branch + worktree under ~/.csm.
    // - not in a git repo: run directly in the current directory.
    // `created_worktree` tracks whether csm owns the worktree so cleanup never
    // touches the user's own directory.
    let (branch, source_repo, worktree, created_worktree) = if here {
        let cwd = std::env::current_dir()
            .context("Could not determine current directory")?
            .to_string_lossy()
            .to_string();
        // Prefer the repo root as the source repo for display purposes; fall
        // back to the cwd when not in a git repository.
        let source_repo = git::repo_root().unwrap_or_else(|_| cwd.clone());
        eprintln!("Running directly in '{cwd}' without a worktree.");
        (String::new(), source_repo, cwd, false)
    } else {
        match git::repo_root().ok() {
            Some(source_repo) => {
                // On a default branch (main/master), pull latest before branching
                // so the new worktree starts from up-to-date history.
                if let Some(current) = git::current_branch(&source_repo)
                    && (current == "main" || current == "master")
                {
                    eprintln!("On default branch '{current}', pulling latest changes...");
                    if let Err(e) = git::pull(&source_repo) {
                        eprintln!("Warning: {e}");
                    }
                }

                let branch = format!("{BRANCH_PREFIX}/{name}");
                let repo_name = git::repo_name(&source_repo);
                let worktree_path = dir
                    .join("worktrees")
                    .join(&repo_name)
                    .join(format!("{repo_name}-{zellij_name}"));

                // Defense in depth: ensure the constructed path lives under ~/.csm.
                if !worktree_path.starts_with(&dir) {
                    bail!(
                        "Refusing to create worktree outside of {}: {}",
                        dir.display(),
                        worktree_path.display()
                    );
                }
                let worktree = worktree_path.to_string_lossy().to_string();

                let new_branch = !git::branch_exists(&branch, None);
                // If the branch already exists, warn and confirm before resuming
                // it, since silently reusing old branch history is confusing.
                if !new_branch
                    && !confirm(&format!(
                        "Branch '{branch}' already exists and will be resumed in a new worktree. Continue?"
                    ))
                {
                    bail!("Aborted: branch '{branch}' already exists.");
                }
                git::create_worktree(&worktree, &branch, new_branch, None)?;
                (branch, source_repo, worktree, true)
            }
            None => {
                let cwd = std::env::current_dir()
                    .context("Could not determine current directory")?
                    .to_string_lossy()
                    .to_string();
                eprintln!(
                    "Not in a git repository; running in current directory without a worktree."
                );
                (String::new(), cwd.clone(), cwd, false)
            }
        }
    };

    // Only include the gitui tab when the working directory is a git repo;
    // otherwise gitui fails to launch.
    let include_git = git::is_git_repo(&worktree);

    let model = ActiveModel {
        name: Set(session_name.clone()),
        branch: Set(branch.clone()),
        copilot_uuid: Set(uuid.clone()),
        source_repo: Set(source_repo.clone()),
        worktree_path: Set(worktree.clone()),
        status: Set(STATUS_ACTIVE.to_string()),
        last_used_at: Set(now_str()),
    };
    model.insert(&db).await?;

    if branch.is_empty() {
        eprintln!("Created session '{session_name}' (uuid: {uuid})");
    } else {
        eprintln!("Created session '{session_name}' (branch: {branch}, uuid: {uuid})");
    }
    let result = start_zellij_session(
        &db,
        &session_name,
        &zellij_name,
        &uuid,
        &worktree,
        false,
        include_git,
    )
    .await;

    if result.is_err() {
        let _ = session::Entity::delete_by_id(session_name.clone())
            .exec(&db)
            .await;
        if created_worktree && let Err(e) = git::remove_worktree(&source_repo, &worktree) {
            eprintln!("Warning: cleanup after failed run: {e}");
        }
    }
    result
}

pub async fn start(name: &str) -> Result<()> {
    let db = crate::db::connect().await?;
    let session = resolve_session(&db, name).await?;
    let sname = session.name.clone();
    let zname = zellij_session_name(&session);

    if session.status == STATUS_REMOVED {
        bail!("Session '{sname}' has been removed. Use `csm restore {sname}` to recover.");
    }

    let zs = zellij::State::query();
    if zs.is_running(&zname) {
        bail!("Session '{sname}' is already running. Use `csm attach {sname}` to connect.");
    }
    if zs.exists(&zname) {
        zellij::cleanup(&zname);
    }

    let uuid = session.copilot_uuid.clone();
    let worktree = session.worktree_path.clone();

    let mut active: ActiveModel = session.into();
    active.last_used_at = Set(now_str());
    active.update(&db).await?;

    eprintln!("Starting session '{sname}' (uuid: {uuid})");
    let include_git = git::is_git_repo(&worktree);
    start_zellij_session(&db, &sname, &zname, &uuid, &worktree, true, include_git).await
}

pub async fn attach(name: &str) -> Result<()> {
    let db = crate::db::connect().await?;
    let session = resolve_session(&db, name).await?;
    let sname = session.name.clone();
    let zname = zellij_session_name(&session);

    if session.status == STATUS_REMOVED {
        bail!("Session '{sname}' has been removed. Use `csm restore {sname}` to recover.");
    }

    let zs = zellij::State::query();
    if !zs.is_running(&zname) {
        bail!("Session '{sname}' is not running. Use `csm start {sname}` first.");
    }

    let mut active: ActiveModel = session.into();
    active.last_used_at = Set(now_str());
    active.update(&db).await?;

    let mut cmd = Command::new("zellij");
    cmd.args(["attach", zname.as_str()]);
    enter_zellij(&db, &sname, &zname, cmd).await
}

pub async fn stop(names: &[String]) -> Result<()> {
    if names.is_empty() {
        bail!("No session names provided");
    }

    let db = crate::db::connect().await?;
    let zs = zellij::State::query();

    for name in names {
        let session = match resolve_session(&db, name).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{e}, skipping");
                continue;
            }
        };
        let sname = &session.name;
        let zname = zellij_session_name(&session);

        if session.status == STATUS_REMOVED {
            eprintln!("Session '{sname}' has been removed, skipping");
            continue;
        }

        if !zs.is_running(&zname) {
            if zs.exists(&zname) {
                zellij::cleanup(&zname);
                println!("Cleaned up dead session '{sname}'");
            } else {
                println!("Session '{sname}' is not running");
            }
            continue;
        }

        if zellij::stop_and_cleanup(&zname) {
            println!("Stopped session '{sname}'");
        } else {
            eprintln!(
                "Warning: zellij session '{zname}' did not exit within timeout; it may still be present."
            );
        }
    }
    Ok(())
}

pub async fn rm(
    names: &[String],
    force: bool,
    interactive: bool,
    older_than: Option<u64>,
) -> Result<()> {
    let db = crate::db::connect().await?;

    let names: Vec<String> = if interactive {
        let items = interactive_remove_candidates(&db).await?;
        if items.is_empty() {
            println!("No sessions to remove.");
            return Ok(());
        }
        let title = if force {
            "Select sessions to PERMANENTLY destroy"
        } else {
            "Select sessions to remove"
        };
        match interactive::pick(items, title)? {
            Some(v) => v,
            None => {
                println!("Cancelled");
                return Ok(());
            }
        }
    } else {
        if names.is_empty() && older_than.is_none() {
            bail!("No sessions specified: provide names, --interactive, or --older-than <DAYS>");
        }
        names.to_vec()
    };

    let zs = zellij::State::query();
    let csm = csm_dir()?;

    // Build a deduped list of target sessions from explicit/picked names and,
    // if requested, all sessions inactive for at least `older_than` days.
    let mut targets: Vec<session::Model> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for name in &names {
        match resolve_session(&db, name).await {
            Ok(s) => {
                if seen.insert(s.name.clone()) {
                    targets.push(s);
                }
            }
            Err(e) => eprintln!("{e}, skipping"),
        }
    }

    if let Some(days) = older_than {
        let aged = Session::find()
            .filter(Column::Status.ne(STATUS_REMOVED))
            .all(&db)
            .await?;
        for s in aged {
            if days_since(&s.last_used_at).is_some_and(|d| d >= days as i64)
                && seen.insert(s.name.clone())
            {
                targets.push(s);
            }
        }
    }

    if targets.is_empty() {
        println!("No matching sessions to remove.");
        return Ok(());
    }

    for session in targets {
        remove_one(&db, &zs, &csm, session, force).await?;
    }

    Ok(())
}

/// Remove (or, with `force`, destroy) a single resolved session. The worktree
/// directory is only deleted when it lives under `~/.csm` so sessions started
/// in a plain directory (no git repo) never wipe the user's own files.
async fn remove_one(
    db: &DatabaseConnection,
    zs: &zellij::State,
    csm: &std::path::Path,
    session: session::Model,
    force: bool,
) -> Result<()> {
    let sname = session.name.clone();
    let zname = zellij_session_name(&session);

    if session.status == STATUS_REMOVED {
        if force {
            zellij::cleanup_session_files(&session.copilot_uuid);
            session::Entity::delete_by_id(sname.to_string())
                .exec(db)
                .await?;
            println!("Destroyed session '{sname}'");
        } else {
            eprintln!("Session '{sname}' is already removed, skipping (use -f to destroy)");
        }
        return Ok(());
    }

    if (zs.is_running(&zname) || zs.exists(&zname)) && !zellij::stop_and_cleanup(&zname) {
        eprintln!(
            "Warning: zellij session '{zname}' did not exit within timeout; \
             continuing with removal."
        );
    }

    let managed_worktree = std::path::Path::new(&session.worktree_path).starts_with(csm);
    if managed_worktree
        && let Err(e) = git::remove_worktree(&session.source_repo, &session.worktree_path)
    {
        eprintln!("Warning: {e}; continuing with removal of '{sname}'.");
    }

    if force {
        zellij::cleanup_session_files(&session.copilot_uuid);
        session::Entity::delete_by_id(sname.to_string())
            .exec(db)
            .await?;
        println!("Destroyed session '{sname}'");
    } else {
        let mut active: ActiveModel = session.into();
        active.status = Set(STATUS_REMOVED.to_string());
        active.update(db).await?;
        println!("Removed session '{sname}'");
    }
    Ok(())
}

/// Build a sorted, formatted list of sessions for the interactive picker.
/// Active sessions (anything not `STATUS_REMOVED`) are visible by default;
/// already-removed sessions are included as `hidden` items so the picker's
/// `a` keybind can reveal them on demand. This mirrors `csm ls -a`. Removed
/// sessions only have an effect when combined with `-f`, since `rm` without
/// `-f` skips already-removed entries with a warning (see `rm` above).
async fn interactive_remove_candidates(db: &DatabaseConnection) -> Result<Vec<interactive::Item>> {
    let sessions = Session::find()
        .order_by_desc(Column::LastUsedAt)
        .all(db)
        .await?;

    if sessions.is_empty() {
        return Ok(Vec::new());
    }

    let all_hex_ids: Vec<String> = sessions
        .iter()
        .map(|s| display::uuid_hex(&s.copilot_uuid))
        .collect();

    let zs = zellij::State::query();
    let mut entries: Vec<(&session::Model, String)> = sessions
        .iter()
        .map(|s| {
            let status = if s.status == STATUS_REMOVED {
                STATUS_REMOVED.to_string()
            } else {
                let zname = zellij_session_name(s);
                zs.display_status(&zname).to_string()
            };
            (s, status)
        })
        .collect();

    entries.sort_by(|(a, sa), (b, sb)| {
        display::status_rank(sa)
            .cmp(&display::status_rank(sb))
            .then(b.last_used_at.cmp(&a.last_used_at))
    });

    let hex_ids: Vec<String> = entries
        .iter()
        .map(|(s, _)| display::uuid_hex(&s.copilot_uuid))
        .collect();
    let unique_lens = display::shortest_unique_prefixes_within(&hex_ids, &all_hex_ids);

    Ok(entries
        .iter()
        .enumerate()
        .map(|(i, (s, status))| {
            // Use the same colored renderer as `csm ls`. The picker handles
            // embedded ANSI escapes when truncating/padding, and strips them
            // off the cursor row so reverse-video highlighting stays clean.
            let shortcode = display::format_shortcode(&hex_ids[i], unique_lens[i], true);
            let repo = git::repo_name(&s.source_repo);
            let branch = if s.status == STATUS_REMOVED {
                s.branch.clone()
            } else {
                git::current_branch(&s.worktree_path).unwrap_or_else(|| s.branch.clone())
            };
            let display_line = display::format_session_line(
                &shortcode,
                &s.name,
                &repo,
                &branch,
                status,
                &s.last_used_at,
                true,
            );
            let search_text = format!("{} {} {} {} {}", s.name, repo, branch, status, hex_ids[i]);
            interactive::Item {
                key: s.name.clone(),
                display: display_line,
                search_text,
                hidden: s.status == STATUS_REMOVED,
            }
        })
        .collect())
}

pub async fn list(show_all: bool) -> Result<()> {
    let db = crate::db::connect().await?;

    let all_hex_ids: Vec<String> = Session::find()
        .all(&db)
        .await?
        .iter()
        .map(|s| display::uuid_hex(&s.copilot_uuid))
        .collect();

    let sessions = if show_all {
        Session::find()
            .order_by_desc(Column::LastUsedAt)
            .all(&db)
            .await?
    } else {
        Session::find()
            .filter(Column::Status.ne(STATUS_REMOVED))
            .order_by_desc(Column::LastUsedAt)
            .all(&db)
            .await?
    };

    if sessions.is_empty() {
        println!("No sessions found.");
        return Ok(());
    }

    let color = display::use_color();
    let zs = zellij::State::query();

    let mut entries: Vec<(&session::Model, &str)> = sessions
        .iter()
        .map(|s| {
            let status = if s.status == STATUS_REMOVED {
                STATUS_REMOVED
            } else {
                let zname = zellij_session_name(s);
                zs.display_status(&zname)
            };
            (s, status)
        })
        .collect();

    entries.sort_by(|(a, sa), (b, sb)| {
        display::status_rank(sa)
            .cmp(&display::status_rank(sb))
            .then(b.last_used_at.cmp(&a.last_used_at))
    });

    let hex_ids: Vec<String> = entries
        .iter()
        .map(|(s, _)| display::uuid_hex(&s.copilot_uuid))
        .collect();
    let unique_lens = display::shortest_unique_prefixes_within(&hex_ids, &all_hex_ids);

    for (i, (s, status)) in entries.iter().enumerate() {
        let shortcode = display::format_shortcode(&hex_ids[i], unique_lens[i], color);
        let repo = git::repo_name(&s.source_repo);
        let branch = if s.status != STATUS_REMOVED {
            git::current_branch(&s.worktree_path).unwrap_or_else(|| s.branch.clone())
        } else {
            s.branch.clone()
        };
        let line = display::format_session_line(
            &shortcode,
            &s.name,
            &repo,
            &branch,
            status,
            &s.last_used_at,
            color,
        );
        println!("{line}");
    }

    Ok(())
}

pub async fn restore(name: &str) -> Result<()> {
    let db = crate::db::connect().await?;
    let session = resolve_session(&db, name).await?;
    let sname = session.name.clone();
    let zname = zellij_session_name(&session);

    if session.status != STATUS_REMOVED {
        bail!(
            "Session '{sname}' is not removed (status: {}). Use `csm attach` instead.",
            session.status
        );
    }

    if !git::branch_exists(&session.branch, Some(&session.source_repo)) {
        bail!("Branch '{}' no longer exists", session.branch);
    }

    git::create_worktree(
        &session.worktree_path,
        &session.branch,
        false,
        Some(&session.source_repo),
    )?;

    let uuid = session.copilot_uuid.clone();
    let worktree = session.worktree_path.clone();

    let mut active: ActiveModel = session.into();
    active.status = Set(STATUS_ACTIVE.to_string());
    active.last_used_at = Set(now_str());
    active.update(&db).await?;

    eprintln!("Restored session '{sname}' (uuid: {uuid})");
    let include_git = git::is_git_repo(&worktree);
    start_zellij_session(&db, &sname, &zname, &uuid, &worktree, true, include_git).await
}

pub async fn rename(old: &str, new_name: &str) -> Result<()> {
    validate_name(new_name)?;
    let db = crate::db::connect().await?;
    let session = resolve_session(&db, old).await?;
    let old_name = session.name.clone();
    let zname = zellij_session_name(&session);

    if old_name == new_name {
        bail!("New name is the same as the old name");
    }

    // Zellij session name is UUID-based, so it doesn't change on rename.
    // Just update the DB record. Both the existence check and the
    // delete+insert happen inside a single transaction so that two
    // concurrent renames cannot both pass the check and clobber each other.
    let new_session = ActiveModel {
        name: Set(new_name.to_string()),
        branch: Set(session.branch.clone()),
        copilot_uuid: Set(session.copilot_uuid.clone()),
        source_repo: Set(session.source_repo.clone()),
        worktree_path: Set(session.worktree_path.clone()),
        status: Set(session.status.clone()),
        last_used_at: Set(now_str()),
    };

    let txn = db.begin().await?;
    if Session::find_by_id(new_name).one(&txn).await?.is_some() {
        txn.rollback().await?;
        bail!("Session '{new_name}' already exists");
    }
    session::Entity::delete_by_id(old_name.clone())
        .exec(&txn)
        .await?;
    new_session.insert(&txn).await?;
    txn.commit().await?;

    let zs = zellij::State::query();
    let running = if zs.is_running(&zname) {
        " (still running)"
    } else {
        ""
    };
    println!("Renamed session '{old_name}' → '{new_name}'{running}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_name_accepts_valid() {
        assert!(validate_name("abc").is_ok());
        assert!(validate_name("abc-123").is_ok());
        assert!(validate_name("abc_123").is_ok());
        assert!(validate_name("a").is_ok());
    }

    #[test]
    fn validate_name_rejects_empty() {
        assert!(validate_name("").is_err());
    }

    #[test]
    fn validate_name_rejects_special_chars() {
        for bad in ["a b", "a/b", "a.b", "a\\b", "a;b", "a$b"] {
            assert!(
                validate_name(bad).is_err(),
                "expected '{bad}' to be rejected"
            );
        }
    }

    #[test]
    fn days_since_computes_whole_days() {
        let fmt = "%Y-%m-%d %H:%M:%S";
        let now = Utc::now().naive_utc();
        let three_days_ago = (now - chrono::Duration::days(3)).format(fmt).to_string();
        assert_eq!(days_since(&three_days_ago), Some(3));

        let now_ts = now.format(fmt).to_string();
        assert_eq!(days_since(&now_ts), Some(0));

        assert_eq!(days_since("not a timestamp"), None);
    }
}
