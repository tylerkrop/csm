use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, TransactionTrait};
use uuid::Uuid;

use crate::display;
use crate::entity::session::{self, ActiveModel, Column, Entity as Session};
use crate::git;
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

fn copilot_command(uuid: &str) -> Result<String> {
    if !uuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        bail!("Invalid UUID format: {uuid}");
    }
    Ok(format!("copilot --yolo --no-remote --autopilot --resume={uuid}"))
}

/// The zellij session name is the 8-char hex prefix of the copilot UUID.
fn zellij_session_name(session: &session::Model) -> String {
    display::short_uuid(&session.copilot_uuid)
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
        n => bail!("Ambiguous identifier '{query}' matches {n} sessions"),
    }
}

/// Run the zellij client, then update last_used_at when the user detaches.
/// Aborts the command injector (if any) if zellij fails to start.
async fn enter_zellij(
    db: &DatabaseConnection,
    session_name: &str,
    mut cmd: Command,
    injector: Option<tokio::task::JoinHandle<()>>,
) -> Result<()> {
    let result = cmd.status().context("Failed to run zellij");

    if result.is_err() {
        if let Some(handle) = injector {
            handle.abort();
        }
        return result.map(|_| ());
    }

    // User detached — update last_used_at
    if let Some(session) = Session::find_by_id(session_name).one(db).await? {
        let mut active: ActiveModel = session.into();
        active.last_used_at = Set(now_str());
        active.update(db).await?;
    }
    Ok(())
}

// ── Commands ────────────────────────────────────────────────────────────────

pub async fn run(name: &str) -> Result<()> {
    validate_name(name)?;
    let db = crate::db::connect().await?;
    let dir = csm_dir()?;

    if let Some(existing) = Session::find_by_id(name).one(&db).await? {
        if existing.status == STATUS_ACTIVE {
            bail!("Session '{name}' already exists. Use `csm attach {name}` to connect.");
        }
        session::Entity::delete_by_id(name.to_string())
            .exec(&db)
            .await?;
    }

    let branch = format!("{BRANCH_PREFIX}/{name}");
    let uuid = Uuid::new_v4().to_string();
    let source_repo = git::repo_root()?;
    let repo_name = git::repo_name(&source_repo);
    let zellij_name = display::short_uuid(&uuid);
    let worktree = dir
        .join("worktrees")
        .join(&repo_name)
        .join(format!("{repo_name}-{zellij_name}"))
        .to_string_lossy()
        .to_string();

    let new_branch = !git::branch_exists(&branch, None);
    git::create_worktree(&worktree, &branch, new_branch, None)?;

    let model = ActiveModel {
        name: Set(name.to_string()),
        branch: Set(branch.clone()),
        copilot_uuid: Set(uuid.clone()),
        source_repo: Set(source_repo.clone()),
        worktree_path: Set(worktree.clone()),
        status: Set("active".to_string()),
        last_used_at: Set(now_str()),
    };
    model.insert(&db).await?;

    eprintln!("Created session '{name}' (branch: {branch}, uuid: {uuid})");
    let injector = zellij::spawn_command_injector(zellij_name.clone(), copilot_command(&uuid)?);
    let mut cmd = Command::new("zellij");
    cmd.args(["-s", zellij_name.as_str()]).current_dir(&worktree);
    let result = enter_zellij(&db, name, cmd, Some(injector)).await;

    if result.is_err() {
        let _ = session::Entity::delete_by_id(name.to_string())
            .exec(&db)
            .await;
        git::remove_worktree(&source_repo, &worktree);
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
    let injector = zellij::spawn_command_injector(zname.clone(), copilot_command(&uuid)?);
    let mut cmd = Command::new("zellij");
    cmd.args(["-s", zname.as_str()]).current_dir(&worktree);
    enter_zellij(&db, &sname, cmd, Some(injector)).await
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
    enter_zellij(&db, &sname, cmd, None).await
}

pub async fn stop(name: &str) -> Result<()> {
    let db = crate::db::connect().await?;
    let session = resolve_session(&db, name).await?;
    let sname = &session.name;
    let zname = zellij_session_name(&session);

    if session.status == STATUS_REMOVED {
        bail!("Session '{sname}' has been removed");
    }

    let zs = zellij::State::query();

    if !zs.is_running(&zname) {
        if zs.exists(&zname) {
            zellij::cleanup(&zname);
            println!("Cleaned up dead session '{sname}'");
        } else {
            println!("Session '{sname}' is not running");
        }
        return Ok(());
    }

    zellij::stop(&zname);
    zellij::cleanup(&zname);
    println!("Stopped session '{sname}'");
    Ok(())
}

pub async fn rm(names: &[String], force: bool) -> Result<()> {
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
        let sname = session.name.clone();
        let zname = zellij_session_name(&session);

        if session.status == STATUS_REMOVED {
            if force {
                session::Entity::delete_by_id(sname.to_string())
                    .exec(&db)
                    .await?;
                println!("Destroyed session '{sname}'");
            } else {
                eprintln!("Session '{sname}' is already removed, skipping (use -f to destroy)");
            }
            continue;
        }

        if zs.is_running(&zname) {
            zellij::stop(&zname);
        }
        if zs.exists(&zname) {
            zellij::cleanup(&zname);
        }

        git::remove_worktree(&session.source_repo, &session.worktree_path);

        if force {
            session::Entity::delete_by_id(sname.to_string())
                .exec(&db)
                .await?;
            println!("Destroyed session '{sname}'");
        } else {
            let mut active: ActiveModel = session.into();
            active.status = Set("removed".to_string());
            active.update(&db).await?;
            println!("Removed session '{sname}'");
        }
    }

    Ok(())
}

pub async fn list(show_all: bool) -> Result<()> {
    let db = crate::db::connect().await?;

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
    let unique_lens = display::shortest_unique_prefixes(&hex_ids);

    for (i, (s, status)) in entries.iter().enumerate() {
        let shortcode = display::format_shortcode(&hex_ids[i], unique_lens[i], color);
        let repo = git::repo_name(&s.source_repo);
        let branch = if s.status != STATUS_REMOVED {
            git::current_branch(&s.worktree_path)
                .unwrap_or_else(|| s.branch.clone())
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

    git::create_worktree(&session.worktree_path, &session.branch, false, Some(&session.source_repo))?;

    let uuid = session.copilot_uuid.clone();
    let worktree = session.worktree_path.clone();

    let mut active: ActiveModel = session.into();
    active.status = Set("active".to_string());
    active.last_used_at = Set(now_str());
    active.update(&db).await?;

    eprintln!("Restored session '{sname}' (uuid: {uuid})");
    let injector = zellij::spawn_command_injector(zname.clone(), copilot_command(&uuid)?);
    let mut cmd = Command::new("zellij");
    cmd.args(["-s", zname.as_str()]).current_dir(&worktree);
    enter_zellij(&db, &sname, cmd, Some(injector)).await
}

pub async fn rename(old: &str, new_name: &str) -> Result<()> {
    validate_name(new_name)?;
    let db = crate::db::connect().await?;
    let session = resolve_session(&db, old).await?;
    let old_name = session.name.clone();
    let zname = zellij_session_name(&session);

    if Session::find_by_id(new_name).one(&db).await?.is_some() {
        bail!("Session '{new_name}' already exists");
    }

    // Zellij session name is UUID-based, so it doesn't change on rename.
    // Just update the DB record.
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
    session::Entity::delete_by_id(old_name.clone())
        .exec(&txn)
        .await?;
    new_session.insert(&txn).await?;
    txn.commit().await?;

    let zs = zellij::State::query();
    let running = if zs.is_running(&zname) { " (still running)" } else { "" };
    println!("Renamed session '{old_name}' → '{new_name}'{running}");
    Ok(())
}
