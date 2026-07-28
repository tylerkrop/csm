use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
    QueryOrder, TransactionTrait, sea_query::Expr,
};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::codespace;
use crate::display;
use crate::entity::session::{self, ActiveModel, Column, Entity as Session};
use crate::git;
use crate::interactive;
use crate::zellij;

// ── Constants ───────────────────────────────────────────────────────────────

const STATUS_ACTIVE: &str = "active";
const STATUS_REMOVED: &str = "removed";
const BRANCH_PREFIX: &str = "tylerkrop";
const BACKEND_LOCAL: &str = "local";
const BACKEND_CODESPACE: &str = "codespace";

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

fn log_resolved_session(query: &str, session: &session::Model) {
    debug!(
        session.query = query,
        session.name = %session.name,
        session.uuid = %session.copilot_uuid,
        session.backend = %session.backend,
        codespace.name = session.codespace_name.as_deref().unwrap_or(""),
        "Resolved session"
    );
}

fn record_session_span(session: &session::Model) {
    let span = tracing::Span::current();
    span.record("session.name", tracing::field::display(&session.name));
    span.record(
        "session.uuid",
        tracing::field::display(&session.copilot_uuid),
    );
    span.record("session.backend", tracing::field::display(&session.backend));
}

struct CodespaceDetails<'a> {
    name: &'a str,
    workdir: &'a str,
    github_login: &'a str,
}

fn codespace_details(session: &session::Model) -> Result<CodespaceDetails<'_>> {
    let name = session
        .codespace_name
        .as_deref()
        .context("Codespace session is missing its Codespace name")?;
    let workdir = session
        .remote_workdir
        .as_deref()
        .context("Codespace session is missing its remote workspace")?;
    let github_login = session
        .github_login
        .as_deref()
        .context("Codespace session is missing its GitHub account")?;
    codespace::validate_name(name)?;
    codespace::validate_remote_workdir(workdir)?;
    Ok(CodespaceDetails {
        name,
        workdir,
        github_login,
    })
}

async fn set_codespace_cache(
    db: &DatabaseConnection,
    uuid: &str,
    codespace_state: &str,
    zellij_state: codespace::RemoteZellijState,
) -> Result<()> {
    let session = Session::find()
        .filter(Column::CopilotUuid.eq(uuid))
        .one(db)
        .await?
        .with_context(|| format!("Session with UUID '{uuid}' disappeared"))?;
    let mut active: ActiveModel = session.into();
    active.cached_codespace_state = Set(Some(codespace_state.to_ascii_lowercase()));
    active.cached_zellij_state = Set(Some(zellij_state.as_str().to_string()));
    active.codespace_state_updated_at = Set(Some(now_str()));
    active.update(db).await?;
    Ok(())
}

async fn stop_codespace_and_cache(
    db: &DatabaseConnection,
    codespace_name: &str,
    github_login: &str,
    uuid: &str,
) -> Result<()> {
    codespace::stop(codespace_name, github_login)?;
    set_codespace_cache(db, uuid, "shutdown", codespace::RemoteZellijState::Missing).await
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
        log_resolved_session(query, &s);
        return Ok(s);
    }

    let all = Session::find().all(db).await?;
    let matches: Vec<_> = all
        .into_iter()
        .filter(|s| display::uuid_hex(&s.copilot_uuid).starts_with(query))
        .collect();

    match matches.len() {
        0 => bail!("No session found matching '{query}'"),
        1 => {
            let session = matches.into_iter().next().unwrap();
            log_resolved_session(query, &session);
            Ok(session)
        }
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
async fn enter_local_zellij(
    db: &DatabaseConnection,
    session_name: &str,
    zellij_name: &str,
    uuid: &str,
    mut cmd: Command,
) -> Result<()> {
    debug!(
        session.name = session_name,
        session.uuid = uuid,
        zellij.session = zellij_name,
        command = ?cmd,
        "Launching local Zellij client"
    );
    let status = cmd.status().context("Failed to run zellij")?;
    debug!(
        session.name = session_name,
        session.uuid = uuid,
        zellij.session = zellij_name,
        %status,
        "Local Zellij client exited"
    );
    if !status.success() && !zellij::State::query().exists(zellij_name) {
        bail!("zellij exited with {status} before session '{zellij_name}' started");
    }

    // User detached or quit — update last_used_at
    let session = Session::find()
        .filter(Column::CopilotUuid.eq(uuid))
        .one(db)
        .await?;
    if let Some(session) = &session {
        let mut active: ActiveModel = session.clone().into();
        active.last_used_at = Set(now_str());
        active.update(db).await?;
    } else {
        warn!(
            session.name = session_name,
            zellij.session = zellij_name,
            "Session missing from database after detach; Zellij session may be orphaned"
        );
    }

    // A detached session remains running. Any other state means the user quit
    // or the session exited, so clean it up.
    let zs = zellij::State::query();
    if !zs.is_running(zellij_name) && zs.exists(zellij_name) {
        zellij::cleanup(zellij_name);
    }

    Ok(())
}

/// Launch a fresh zellij session whose `ai` tab runs the copilot launcher.
/// Used by `run`, `start`, and `restore`, which share the same startup shape.
/// The launcher itself picks `copilot --session-id` (first launch) vs
/// `copilot --resume` (subsequent launches) via a per-session marker, so csm no
/// longer types the command into the pane. Pass `resume = true` when relaunching
/// an existing session (`start`/`restore`) so the marker is ensured up front and
/// the launcher resumes even for sessions created before the launcher existed;
/// pass `resume = false` for a brand-new session (`run`), letting the launcher
/// create the marker on its first `--session-id` launch.
async fn start_local_zellij_session(
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
    enter_local_zellij(db, session_name, zellij_name, uuid, cmd).await
}

async fn enter_codespace_zellij(
    db: &DatabaseConnection,
    session: &session::Model,
    attach_only: bool,
) -> Result<()> {
    let details = codespace_details(session)?;
    let codespace_name = details.name.to_string();
    let remote_workdir = details.workdir.to_string();
    let github_login = details.github_login.to_string();
    let uuid = session.copilot_uuid.clone();
    let session_name = session.name.clone();
    set_codespace_cache(
        db,
        &uuid,
        "available",
        codespace::RemoteZellijState::Connecting,
    )
    .await?;
    let status = match codespace::connect_zellij(
        &codespace_name,
        &remote_workdir,
        &uuid,
        &github_login,
        attach_only,
    ) {
        Ok(status) => status,
        Err(error) => {
            let zellij_state = if attach_only {
                codespace::RemoteZellijState::Running
            } else {
                codespace::RemoteZellijState::Missing
            };
            set_codespace_cache(db, &uuid, "available", zellij_state).await?;
            return Err(error);
        }
    };

    if let Some(session) = Session::find()
        .filter(Column::CopilotUuid.eq(&uuid))
        .one(db)
        .await?
    {
        let mut active: ActiveModel = session.into();
        active.last_used_at = Set(now_str());
        active.update(db).await?;
    } else {
        warn!(
            session.name = session_name,
            session.uuid = uuid,
            codespace.name = codespace_name,
            "Session missing from database after remote Zellij exited"
        );
    }

    let codespace_state = codespace::current_state(&codespace_name, &github_login)?;
    if codespace_state.eq_ignore_ascii_case("shutdown") {
        set_codespace_cache(db, &uuid, "shutdown", codespace::RemoteZellijState::Missing).await?;
        return Ok(());
    }

    let state_after = codespace::remote_zellij_state(&codespace_name, &uuid, &github_login)?;
    match state_after {
        codespace::RemoteZellijState::Connecting => {
            bail!("Remote Zellij state query returned an internal connecting state")
        }
        codespace::RemoteZellijState::Running => {
            if !codespace::remote_zellij_ready(&codespace_name, &uuid, &github_login)? {
                let _ = stop_codespace_and_cache(db, &codespace_name, &github_login, &uuid).await;
                bail!("Remote Zellij is running without the required ai/git/edit layout")
            }
            set_codespace_cache(db, &uuid, &codespace_state, state_after).await?;
            if !status.success() {
                warn!(
                    session.uuid = uuid,
                    zellij.session = display::short_uuid(&uuid),
                    %status,
                    "SSH exited but remote Zellij session is still running"
                );
            }
            Ok(())
        }
        codespace::RemoteZellijState::Exited => {
            let _ = codespace::cleanup_remote_zellij(&codespace_name, &uuid, &github_login);
            info!(
                codespace.name = codespace_name,
                session.uuid = uuid,
                "Stopping Codespace after remote Zellij exited"
            );
            stop_codespace_and_cache(db, &codespace_name, &github_login, &uuid).await?;
            if status.success() {
                Ok(())
            } else {
                bail!("SSH exited with {status} after remote Zellij exited")
            }
        }
        codespace::RemoteZellijState::Missing => {
            info!(
                codespace.name = codespace_name,
                session.uuid = uuid,
                "Stopping Codespace after remote Zellij closed"
            );
            stop_codespace_and_cache(db, &codespace_name, &github_login, &uuid).await?;
            if status.success() {
                Ok(())
            } else {
                bail!("SSH exited with {status} and remote Zellij is missing")
            }
        }
    }
}

async fn cleanup_failed_codespace_creation(
    db: &DatabaseConnection,
    session_name: &str,
    uuid: &str,
    repo: &codespace::RepoInfo,
    codespace_name: &str,
    remote_workdir: &str,
    github_login: &str,
) {
    let delete_error = match codespace::delete(codespace_name, github_login) {
        Ok(()) => {
            if let Err(error) = Session::delete_many()
                .filter(Column::Name.eq(session_name))
                .filter(Column::CopilotUuid.eq(uuid))
                .exec(db)
                .await
            {
                warn!(
                    session.name = session_name,
                    session.uuid = uuid,
                    codespace.name = codespace_name,
                    %error,
                    "Deleted Codespace but failed to remove its session record"
                );
            }
            zellij::cleanup_session_files(uuid);
            return;
        }
        Err(error) => error,
    };

    let current_row = match Session::find_by_id(session_name).one(db).await {
        Ok(row) => row,
        Err(error) => {
            warn!(
                session.name = session_name,
                session.uuid = uuid,
                codespace.name = codespace_name,
                %delete_error,
                %error,
                "Failed to delete Codespace and inspect its cleanup record; manual deletion may be required"
            );
            return;
        }
    };
    if current_row
        .as_ref()
        .is_some_and(|session| session.copilot_uuid == uuid)
    {
        warn!(
            session.name = session_name,
            session.uuid = uuid,
            codespace.name = codespace_name,
            %delete_error,
            "Failed to delete Codespace; retained session so cleanup can be retried with csm remove -f"
        );
        return;
    }

    let recovery_name = if current_row.is_none() {
        session_name.to_string()
    } else {
        let base = format!("{session_name}-cleanup-{}", display::short_uuid(uuid));
        match next_available_name(db, &base).await {
            Ok(name) => name,
            Err(error) => {
                warn!(
                    session.name = session_name,
                    session.uuid = uuid,
                    codespace.name = codespace_name,
                    %delete_error,
                    %error,
                    "Failed to delete untracked Codespace or reserve a cleanup record; manual deletion may be required"
                );
                return;
            }
        }
    };

    let recovery = ActiveModel {
        name: Set(recovery_name.clone()),
        branch: Set(repo.default_branch.clone()),
        copilot_uuid: Set(uuid.to_string()),
        source_repo: Set(repo.name_with_owner.clone()),
        worktree_path: Set(String::new()),
        backend: Set(BACKEND_CODESPACE.to_string()),
        codespace_name: Set(Some(codespace_name.to_string())),
        remote_workdir: Set(Some(remote_workdir.to_string())),
        github_login: Set(Some(github_login.to_string())),
        cached_codespace_state: Set(None),
        cached_codespace_branch: Set(Some(repo.default_branch.clone())),
        cached_zellij_state: Set(None),
        codespace_state_updated_at: Set(None),
        status: Set(STATUS_REMOVED.to_string()),
        last_used_at: Set(now_str()),
    };
    match recovery.insert(db).await {
        Ok(_) => warn!(
            session.name = recovery_name,
            session.uuid = uuid,
            codespace.name = codespace_name,
            %delete_error,
            "Failed to delete Codespace; retained cleanup record for csm remove -f"
        ),
        Err(error) => warn!(
            session.name = session_name,
            session.uuid = uuid,
            codespace.name = codespace_name,
            %delete_error,
            %error,
            "Failed to delete untracked Codespace or persist a cleanup record; manual deletion may be required"
        ),
    }
}

async fn run_codespace(db: &DatabaseConnection, session_name: &str, uuid: &str) -> Result<()> {
    let repo_root =
        git::repo_root().context("Codespace sessions must be created from a Git repository")?;
    codespace::check_auth()?;
    let github_login = codespace::current_login()?;
    let repo = codespace::repo_info(&repo_root)?;
    let remote_workdir = codespace::remote_workdir(&repo.name_with_owner)?;

    info!(
        session.name = session_name,
        session.uuid = uuid,
        repository = repo.name_with_owner,
        branch = repo.default_branch,
        "Creating Codespace from default branch"
    );
    let codespace_name = codespace::create(&repo, session_name, uuid)?;
    info!(
        session.name = session_name,
        session.uuid = uuid,
        codespace.name = codespace_name,
        "Created Codespace; preparing remote environment"
    );

    let model = ActiveModel {
        name: Set(session_name.to_string()),
        branch: Set(repo.default_branch.clone()),
        copilot_uuid: Set(uuid.to_string()),
        source_repo: Set(repo.name_with_owner.clone()),
        worktree_path: Set(String::new()),
        backend: Set(BACKEND_CODESPACE.to_string()),
        codespace_name: Set(Some(codespace_name.clone())),
        remote_workdir: Set(Some(remote_workdir.clone())),
        github_login: Set(Some(github_login.clone())),
        cached_codespace_state: Set(Some("available".to_string())),
        cached_codespace_branch: Set(Some(repo.default_branch.clone())),
        cached_zellij_state: Set(Some("missing".to_string())),
        codespace_state_updated_at: Set(Some(now_str())),
        status: Set(STATUS_ACTIVE.to_string()),
        last_used_at: Set(now_str()),
    };
    let session = match model.insert(db).await {
        Ok(session) => session,
        Err(error) => {
            cleanup_failed_codespace_creation(
                db,
                session_name,
                uuid,
                &repo,
                &codespace_name,
                &remote_workdir,
                &github_login,
            )
            .await;
            return Err(error.into());
        }
    };

    let setup_result = (|| -> Result<()> {
        let launcher = zellij::ensure_codespace_launcher()?;
        let layout = zellij::ensure_codespace_layout(uuid, &codespace_name)?;
        let config = zellij::ensure_config()?;
        codespace::prepare_remote(codespace::RemoteSetup {
            name: &codespace_name,
            workdir: &remote_workdir,
            launcher: &launcher,
            layout: &layout,
            config: &config,
            uuid,
            resume: false,
            github_login: &github_login,
        })
    })();
    if let Err(error) = setup_result {
        cleanup_failed_codespace_creation(
            db,
            session_name,
            uuid,
            &repo,
            &codespace_name,
            &remote_workdir,
            &github_login,
        )
        .await;
        return Err(error);
    }

    info!(
        session.name = session_name,
        session.uuid = uuid,
        codespace.name = codespace_name,
        "Created Codespace session"
    );
    info!(
        session.name = session_name,
        session.uuid = uuid,
        codespace.name = codespace_name,
        "Connecting directly to remote Zellij"
    );
    match enter_codespace_zellij(db, &session, false).await {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Err(stop_error) =
                stop_codespace_and_cache(db, &codespace_name, &github_login, uuid).await
            {
                warn!(
                    session.name = session_name,
                    session.uuid = uuid,
                    codespace.name = codespace_name,
                    error = %stop_error,
                    "Failed to stop Codespace after connection failed"
                );
            }
            Err(error)
        }
    }
}

// ── Commands ────────────────────────────────────────────────────────────────

pub async fn run(name: &str, here: bool, use_codespace: bool) -> Result<()> {
    validate_name(name)?;
    let db = crate::db::connect().await?;

    // Resolve the DB session name (primary key). Removed local sessions are
    // reclaimed, while live sessions and retained Codespaces are disambiguated.
    // The local branch still derives from the requested name.
    let session_name = match Session::find_by_id(name).one(&db).await? {
        Some(existing) if existing.status == STATUS_ACTIVE => {
            let unique = next_available_name(&db, name).await?;
            info!(
                session.requested_name = name,
                session.name = unique,
                "Session name is already in use; selected a unique name"
            );
            unique
        }
        Some(existing) if existing.backend == BACKEND_CODESPACE => {
            let unique = next_available_name(&db, name).await?;
            info!(
                session.requested_name = name,
                session.name = unique,
                "Removed Codespace session is still retained; selected a unique name"
            );
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
    let backend = if use_codespace {
        BACKEND_CODESPACE
    } else {
        BACKEND_LOCAL
    };
    let span = tracing::Span::current();
    span.record("session.name", tracing::field::display(&session_name));
    span.record("session.uuid", tracing::field::display(&uuid));
    span.record("session.backend", tracing::field::display(backend));
    debug!(
        session.requested_name = name,
        session.name = session_name,
        session.uuid = uuid,
        session.backend = backend,
        "Allocated session identity"
    );

    if use_codespace {
        return run_codespace(&db, &session_name, &uuid).await;
    }

    let dir = csm_dir()?;

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
        info!(
            session.name = session_name,
            session.uuid = uuid,
            worktree.path = cwd,
            "Running directly without a worktree"
        );
        (String::new(), source_repo, cwd, false)
    } else {
        match git::repo_root().ok() {
            Some(source_repo) => {
                // On a default branch (main/master), pull latest before branching
                // so the new worktree starts from up-to-date history.
                if let Some(current) = git::current_branch(&source_repo)
                    && (current == "main" || current == "master")
                {
                    info!(
                        session.name = session_name,
                        session.uuid = uuid,
                        branch = current,
                        "Pulling latest changes from default branch"
                    );
                    if let Err(e) = git::pull(&source_repo) {
                        warn!(
                            session.name = session_name,
                            session.uuid = uuid,
                            error = %e,
                            "Failed to pull latest changes; continuing"
                        );
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
                info!(
                    session.name = session_name,
                    session.uuid = uuid,
                    worktree.path = cwd,
                    "Not in a Git repository; running without a worktree"
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
        backend: Set(BACKEND_LOCAL.to_string()),
        codespace_name: Set(None),
        remote_workdir: Set(None),
        github_login: Set(None),
        cached_codespace_state: Set(None),
        cached_codespace_branch: Set(None),
        cached_zellij_state: Set(None),
        codespace_state_updated_at: Set(None),
        status: Set(STATUS_ACTIVE.to_string()),
        last_used_at: Set(now_str()),
    };
    model.insert(&db).await?;

    if branch.is_empty() {
        info!(
            session.name = session_name,
            session.uuid = uuid,
            "Created session"
        );
    } else {
        info!(
            session.name = session_name,
            session.uuid = uuid,
            branch,
            "Created session"
        );
    }
    let result = start_local_zellij_session(
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
        // Reap the layout/marker files start_zellij_session wrote before failing
        // so a failed run never leaves an orphaned per-session file behind.
        zellij::cleanup_session_files(&uuid);
        if created_worktree && let Err(e) = git::remove_worktree(&source_repo, &worktree) {
            warn!(
                session.name = session_name,
                session.uuid = uuid,
                error = %e,
                "Cleanup after failed run did not complete"
            );
        }
    }
    result
}

pub async fn start(name: &str) -> Result<()> {
    let db = crate::db::connect().await?;
    let session = resolve_session(&db, name).await?;
    record_session_span(&session);
    let sname = session.name.clone();
    let zname = zellij_session_name(&session);

    if session.status == STATUS_REMOVED {
        bail!("Session '{sname}' has been removed. Use `csm restore {sname}` to recover.");
    }

    let uuid = session.copilot_uuid.clone();
    match session.backend.as_str() {
        BACKEND_LOCAL => {
            let zs = zellij::State::query();
            if zs.is_running(&zname) {
                bail!("Session '{sname}' is already running. Use `csm attach {sname}` to connect.");
            }
            if zs.exists(&zname) {
                zellij::cleanup(&zname);
            }

            let worktree = session.worktree_path.clone();
            let mut active: ActiveModel = session.into();
            active.last_used_at = Set(now_str());
            active.update(&db).await?;

            info!(
                session.name = sname,
                session.uuid = uuid,
                session.backend = BACKEND_LOCAL,
                "Starting session"
            );
            let include_git = git::is_git_repo(&worktree);
            start_local_zellij_session(&db, &sname, &zname, &uuid, &worktree, true, include_git)
                .await
        }
        BACKEND_CODESPACE => {
            let details = codespace_details(&session)?;
            let codespace_name = details.name.to_string();
            let remote_workdir = details.workdir.to_string();
            let github_login = details.github_login.to_string();
            let initial_state = codespace::current_state(&codespace_name, &github_login)?;
            let setup_result = (|| -> Result<()> {
                let launcher = zellij::ensure_codespace_launcher()?;
                let layout = zellij::ensure_codespace_layout(&uuid, &codespace_name)?;
                let config = zellij::ensure_config()?;
                codespace::prepare_remote(codespace::RemoteSetup {
                    name: &codespace_name,
                    workdir: &remote_workdir,
                    launcher: &launcher,
                    layout: &layout,
                    config: &config,
                    uuid: &uuid,
                    resume: true,
                    github_login: &github_login,
                })
            })();
            if let Err(error) = setup_result {
                if initial_state.eq_ignore_ascii_case("shutdown")
                    && let Err(stop_error) =
                        stop_codespace_and_cache(&db, &codespace_name, &github_login, &uuid).await
                {
                    warn!(
                        session.name = sname,
                        session.uuid = uuid,
                        codespace.name = codespace_name,
                        error = %stop_error,
                        "Failed to stop Codespace after setup failed"
                    );
                }
                return Err(error);
            }

            let remote_state =
                match codespace::remote_zellij_state(&codespace_name, &uuid, &github_login) {
                    Ok(state) => state,
                    Err(error) => {
                        if initial_state.eq_ignore_ascii_case("shutdown")
                            && let Err(stop_error) =
                                stop_codespace_and_cache(&db, &codespace_name, &github_login, &uuid)
                                    .await
                        {
                            warn!(
                                session.name = sname,
                                session.uuid = uuid,
                                codespace.name = codespace_name,
                                error = %stop_error,
                                "Failed to stop Codespace after state check failed"
                            );
                        }
                        return Err(error);
                    }
                };
            if remote_state == codespace::RemoteZellijState::Running {
                set_codespace_cache(
                    &db,
                    &uuid,
                    "available",
                    codespace::RemoteZellijState::Running,
                )
                .await?;
                bail!("Session '{sname}' is already running. Use `csm attach {sname}` to connect.");
            }
            let mut active: ActiveModel = session.clone().into();
            active.last_used_at = Set(now_str());
            if let Err(error) = active.update(&db).await {
                if initial_state.eq_ignore_ascii_case("shutdown")
                    && let Err(stop_error) =
                        stop_codespace_and_cache(&db, &codespace_name, &github_login, &uuid).await
                {
                    warn!(
                        session.name = sname,
                        session.uuid = uuid,
                        codespace.name = codespace_name,
                        error = %stop_error,
                        "Failed to stop Codespace after database update failed"
                    );
                }
                return Err(error.into());
            }

            info!(
                session.name = sname,
                session.uuid = uuid,
                session.backend = BACKEND_CODESPACE,
                codespace.name = codespace_name,
                "Starting remote Zellij session"
            );
            match enter_codespace_zellij(&db, &session, false).await {
                Ok(()) => Ok(()),
                Err(error) => {
                    if let Err(stop_error) =
                        stop_codespace_and_cache(&db, &codespace_name, &github_login, &uuid).await
                    {
                        warn!(
                            session.name = sname,
                            session.uuid = uuid,
                            codespace.name = codespace_name,
                            error = %stop_error,
                            "Failed to stop Codespace after start failed"
                        );
                    }
                    Err(error)
                }
            }
        }
        backend => bail!("Session '{sname}' has unknown backend '{backend}'"),
    }
}

pub async fn attach(name: &str) -> Result<()> {
    let db = crate::db::connect().await?;
    let session = resolve_session(&db, name).await?;
    record_session_span(&session);
    let sname = session.name.clone();
    let zname = zellij_session_name(&session);

    if session.status == STATUS_REMOVED {
        bail!("Session '{sname}' has been removed. Use `csm restore {sname}` to recover.");
    }

    let uuid = session.copilot_uuid.clone();
    match session.backend.as_str() {
        BACKEND_LOCAL => {
            let zs = zellij::State::query();
            if !zs.is_running(&zname) {
                bail!("Session '{sname}' is not running. Use `csm start {sname}` first.");
            }

            let mut active: ActiveModel = session.into();
            active.last_used_at = Set(now_str());
            active.update(&db).await?;

            let mut cmd = Command::new("zellij");
            cmd.args(["attach", zname.as_str()]);
            info!(
                session.name = sname,
                session.uuid = uuid,
                session.backend = BACKEND_LOCAL,
                zellij.session = zname,
                "Attaching to local Zellij session"
            );
            enter_local_zellij(&db, &sname, &zname, &uuid, cmd).await
        }
        BACKEND_CODESPACE => {
            let details = codespace_details(&session)?;
            let codespace_state = codespace::current_state(details.name, details.github_login)?;
            if codespace_state.eq_ignore_ascii_case("shutdown") {
                bail!("Session '{sname}' is stopped. Use `csm start {sname}` first.");
            }

            let launcher = zellij::ensure_codespace_launcher()?;
            let layout = zellij::ensure_codespace_layout(&uuid, details.name)?;
            let config = zellij::ensure_config()?;
            codespace::prepare_remote(codespace::RemoteSetup {
                name: details.name,
                workdir: details.workdir,
                launcher: &launcher,
                layout: &layout,
                config: &config,
                uuid: &uuid,
                resume: true,
                github_login: details.github_login,
            })?;
            match codespace::remote_zellij_state(details.name, &uuid, details.github_login)? {
                codespace::RemoteZellijState::Running => {}
                _ => {
                    bail!("Session '{sname}' is not running. Use `csm start {sname}` first.");
                }
            }

            info!(
                session.name = sname,
                session.uuid = uuid,
                session.backend = BACKEND_CODESPACE,
                codespace.name = details.name,
                "Attaching to remote Zellij session"
            );
            enter_codespace_zellij(&db, &session, true).await
        }
        backend => bail!("Session '{sname}' has unknown backend '{backend}'"),
    }
}

pub async fn stop(names: &[String]) -> Result<()> {
    if names.is_empty() {
        bail!("No session names provided");
    }

    let db = crate::db::connect().await?;
    let zs = zellij::State::query();
    let mut failures = 0;

    for name in names {
        let session = match resolve_session(&db, name).await {
            Ok(s) => s,
            Err(e) => {
                warn!(session.query = name, error = %e, "Could not resolve session; skipping");
                continue;
            }
        };
        let sname = &session.name;
        let zname = zellij_session_name(&session);

        if session.status == STATUS_REMOVED {
            warn!(
                session.name = sname,
                session.uuid = session.copilot_uuid,
                "Session has been removed; skipping"
            );
            continue;
        }

        let had_local_session = zs.is_running(&zname) || zs.exists(&zname);
        if had_local_session && !zellij::stop_and_cleanup(&zname) {
            warn!(
                session.name = sname,
                session.uuid = session.copilot_uuid,
                zellij.session = zname,
                "Zellij session did not exit within timeout and may still be present"
            );
        }

        match session.backend.as_str() {
            BACKEND_LOCAL => {
                if zs.is_running(&zname) {
                    info!(
                        target: "csm::result",
                        session_name = sname,
                        session_uuid = session.copilot_uuid,
                        "Stopped session"
                    );
                } else if zs.exists(&zname) {
                    info!(
                        target: "csm::result",
                        session_name = sname,
                        session_uuid = session.copilot_uuid,
                        "Cleaned up exited session"
                    );
                } else {
                    info!(
                        target: "csm::result",
                        session_name = sname,
                        session_uuid = session.copilot_uuid,
                        "Session is not running"
                    );
                }
            }
            BACKEND_CODESPACE => {
                let details = match codespace_details(&session) {
                    Ok(details) => details,
                    Err(error) => {
                        warn!(
                            session.name = sname,
                            session.uuid = session.copilot_uuid,
                            %error,
                            "Codespace session details are invalid"
                        );
                        failures += 1;
                        continue;
                    }
                };
                if let Err(error) = stop_codespace_and_cache(
                    &db,
                    details.name,
                    details.github_login,
                    &session.copilot_uuid,
                )
                .await
                {
                    warn!(
                        session.name = sname,
                        session.uuid = session.copilot_uuid,
                        codespace.name = details.name,
                        %error,
                        "Failed to stop Codespace session"
                    );
                    failures += 1;
                } else {
                    info!(
                        target: "csm::result",
                        session_name = sname,
                        session_uuid = session.copilot_uuid,
                        codespace_name = details.name,
                        "Stopped Codespace session"
                    );
                }
            }
            backend => {
                warn!(
                    session.name = sname,
                    session.uuid = session.copilot_uuid,
                    %backend,
                    "Session has unknown backend"
                );
                failures += 1;
            }
        }
    }

    if failures > 0 {
        bail!("Failed to stop {failures} session(s)");
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
            info!(target: "csm::result", "No sessions to remove");
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
                info!(target: "csm::result", "Removal cancelled");
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
            Err(e) => {
                warn!(session.query = name, error = %e, "Could not resolve session; skipping")
            }
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
        info!(target: "csm::result", "No matching sessions to remove");
    } else {
        for session in targets {
            remove_one(&db, &zs, &csm, session, force).await?;
        }
    }

    // Sweep any per-session layout/marker files that no longer belong to a
    // known session. This reaps files left by older csm versions (which named
    // layouts by shortcode, so `cleanup_session_files` never matched them) and
    // by failure paths that predate cleanup. Run against every remaining
    // session (any status) so restorable sessions keep their files.
    let known: Vec<String> = Session::find()
        .all(&db)
        .await?
        .into_iter()
        .map(|s| s.copilot_uuid)
        .collect();
    let pruned = zellij::prune_orphans(&known);
    if pruned > 0 {
        info!(
            target: "csm::result",
            file_count = pruned,
            "Pruned orphaned session files"
        );
    }

    Ok(())
}

async fn delete_session_by_uuid(db: &DatabaseConnection, uuid: &str) -> Result<()> {
    let txn = db.begin().await?;
    let matches = Session::find()
        .filter(Column::CopilotUuid.eq(uuid))
        .all(&txn)
        .await?;
    if matches.len() != 1 {
        txn.rollback().await?;
        bail!(
            "Expected one session with UUID '{uuid}', found {}",
            matches.len()
        );
    }
    let result = Session::delete_many()
        .filter(Column::CopilotUuid.eq(uuid))
        .exec(&txn)
        .await?;
    match result.rows_affected {
        1 => {
            txn.commit().await?;
            Ok(())
        }
        0 => {
            txn.rollback().await?;
            bail!("Session with UUID '{uuid}' disappeared during removal")
        }
        count => {
            txn.rollback().await?;
            bail!("Found {count} sessions with duplicate UUID '{uuid}'")
        }
    }
}

async fn mark_session_removed(db: &DatabaseConnection, uuid: &str) -> Result<()> {
    let session = Session::find()
        .filter(Column::CopilotUuid.eq(uuid))
        .one(db)
        .await?
        .with_context(|| format!("Session with UUID '{uuid}' disappeared"))?;
    let mut active: ActiveModel = session.into();
    active.status = Set(STATUS_REMOVED.to_string());
    active.update(db).await?;
    Ok(())
}

/// Remove (or, with `force`, destroy) a single resolved session.
async fn remove_one(
    db: &DatabaseConnection,
    zs: &zellij::State,
    csm: &std::path::Path,
    session: session::Model,
    force: bool,
) -> Result<()> {
    let sname = session.name.clone();
    let uuid = session.copilot_uuid.clone();
    let zname = zellij_session_name(&session);
    let codespace_identity = if session.backend == BACKEND_CODESPACE {
        let details = codespace_details(&session)?;
        Some((details.name.to_string(), details.github_login.to_string()))
    } else {
        None
    };

    if session.status == STATUS_REMOVED {
        if force {
            match session.backend.as_str() {
                BACKEND_LOCAL => {}
                BACKEND_CODESPACE => {
                    let (name, github_login) = codespace_identity
                        .as_ref()
                        .context("Codespace session is missing its Codespace name")?;
                    codespace::delete_if_exists(name, github_login)?;
                }
                backend => bail!("Session '{sname}' has unknown backend '{backend}'"),
            }
            zellij::cleanup_session_files(&uuid);
            delete_session_by_uuid(db, &uuid).await?;
            info!(
                target: "csm::result",
                session_name = sname,
                session_uuid = uuid,
                "Destroyed session"
            );
        } else {
            warn!(
                session.name = sname,
                session.uuid = uuid,
                "Session is already removed; skipping unless force is used"
            );
        }
        return Ok(());
    }

    if (zs.is_running(&zname) || zs.exists(&zname)) && !zellij::stop_and_cleanup(&zname) {
        warn!(
            session.name = sname,
            session.uuid = uuid,
            zellij.session = zname,
            "Zellij session did not exit within timeout; continuing with removal"
        );
    }

    match session.backend.as_str() {
        BACKEND_LOCAL => {
            let managed_worktree = std::path::Path::new(&session.worktree_path).starts_with(csm);
            if managed_worktree
                && let Err(error) =
                    git::remove_worktree(&session.source_repo, &session.worktree_path)
            {
                warn!(
                    session.name = sname,
                    session.uuid = uuid,
                    %error,
                    "Failed to remove worktree; continuing with session removal"
                );
            }
        }
        BACKEND_CODESPACE => {
            let (codespace_name, github_login) = codespace_identity
                .as_ref()
                .context("Codespace session is missing its Codespace name")?;
            if force {
                codespace::delete_if_exists(codespace_name, github_login)?;
            } else {
                stop_codespace_and_cache(db, codespace_name, github_login, &uuid).await?;
            }
        }
        backend => bail!("Session '{sname}' has unknown backend '{backend}'"),
    }

    if force {
        zellij::cleanup_session_files(&uuid);
        delete_session_by_uuid(db, &uuid).await?;
        info!(
            target: "csm::result",
            session_name = sname,
            session_uuid = uuid,
            "Destroyed session"
        );
    } else {
        let mut active: ActiveModel = session.into();
        if codespace_identity.is_some() {
            active.cached_codespace_state = Set(Some("shutdown".to_string()));
            active.cached_zellij_state = Set(Some("missing".to_string()));
            active.codespace_state_updated_at = Set(Some(now_str()));
        }
        active.status = Set(STATUS_REMOVED.to_string());
        active.update(db).await?;
        info!(
            target: "csm::result",
            session_name = sname,
            session_uuid = uuid,
            "Removed session"
        );
    }
    Ok(())
}

struct CodespaceStates {
    values: HashMap<String, codespace::RemoteState>,
    zellij_values: HashMap<String, codespace::RemoteZellijState>,
    current_login: Option<String>,
}

fn cached_codespace_states(
    sessions: &[session::Model],
    local_zellij: &zellij::State,
) -> CodespaceStates {
    let mut values = HashMap::new();
    let mut zellij_values = HashMap::new();
    for session in sessions {
        if session.backend != BACKEND_CODESPACE {
            continue;
        }
        let Some(codespace_name) = session.codespace_name.clone() else {
            continue;
        };
        values.insert(
            codespace_name.clone(),
            codespace::RemoteState {
                state: session
                    .cached_codespace_state
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                branch: session.cached_codespace_branch.clone(),
            },
        );
        let zellij_name = zellij_session_name(session);
        let cached_zellij = session
            .cached_zellij_state
            .as_deref()
            .and_then(codespace::RemoteZellijState::from_cached);
        if local_zellij.is_running(&zellij_name) {
            zellij_values.insert(codespace_name, codespace::RemoteZellijState::Running);
        } else if local_zellij.exists(&zellij_name) {
            zellij_values.insert(codespace_name, codespace::RemoteZellijState::Exited);
        } else if let Some(state) = cached_zellij {
            zellij_values.insert(codespace_name, state);
        }
    }
    CodespaceStates {
        values,
        zellij_values,
        current_login: None,
    }
}

fn cache_updated_within(session: &session::Model, seconds: i64) -> bool {
    let Some(updated_at) = session.codespace_state_updated_at.as_deref() else {
        return false;
    };
    let Ok(updated_at) = chrono::NaiveDateTime::parse_from_str(updated_at, "%Y-%m-%d %H:%M:%S")
    else {
        return false;
    };
    let age = Utc::now()
        .naive_utc()
        .signed_duration_since(updated_at)
        .num_seconds();
    (0..=seconds).contains(&age)
}

fn cache_snapshot_matches(left: &session::Model, right: &session::Model) -> bool {
    left.cached_codespace_state == right.cached_codespace_state
        && left.cached_codespace_branch == right.cached_codespace_branch
        && left.cached_zellij_state == right.cached_zellij_state
        && left.codespace_state_updated_at == right.codespace_state_updated_at
}

fn use_session_cache(states: &mut CodespaceStates, codespace_name: &str, session: session::Model) {
    states.values.insert(
        codespace_name.to_string(),
        codespace::RemoteState {
            state: session
                .cached_codespace_state
                .unwrap_or_else(|| "unknown".to_string()),
            branch: session.cached_codespace_branch,
        },
    );
    if let Some(state) = session
        .cached_zellij_state
        .as_deref()
        .and_then(codespace::RemoteZellijState::from_cached)
    {
        states
            .zellij_values
            .insert(codespace_name.to_string(), state);
    } else {
        states.zellij_values.remove(codespace_name);
    }
}

async fn refresh_codespace_states(
    db: &DatabaseConnection,
    sessions: &[session::Model],
    local_zellij: &zellij::State,
) -> Result<CodespaceStates> {
    let mut states = cached_codespace_states(sessions, local_zellij);
    if !sessions
        .iter()
        .any(|session| session.status != STATUS_REMOVED && session.backend == BACKEND_CODESPACE)
    {
        return Ok(states);
    }

    let current_login = codespace::current_login()?;
    let listed = codespace::list_states()?;
    for session in sessions {
        if session.status == STATUS_REMOVED || session.backend != BACKEND_CODESPACE {
            continue;
        }
        let details = codespace_details(session)?;
        if details.github_login != current_login {
            continue;
        }

        let remote = listed
            .get(details.name)
            .cloned()
            .unwrap_or(codespace::RemoteState {
                state: "missing".to_string(),
                branch: session.cached_codespace_branch.clone(),
            });
        let zellij_name = zellij_session_name(session);
        let mut zellij_state = if local_zellij.is_running(&zellij_name) {
            codespace::RemoteZellijState::Running
        } else if remote.state.eq_ignore_ascii_case("available") {
            codespace::remote_zellij_state(
                details.name,
                &session.copilot_uuid,
                details.github_login,
            )?
        } else {
            codespace::RemoteZellijState::Missing
        };
        let latest = Session::find()
            .filter(Column::CopilotUuid.eq(&session.copilot_uuid))
            .one(db)
            .await?
            .with_context(|| {
                format!(
                    "Session with UUID '{}' disappeared during refresh",
                    session.copilot_uuid
                )
            })?;
        if !cache_snapshot_matches(session, &latest) {
            use_session_cache(&mut states, details.name, latest);
            continue;
        }
        if zellij_state == codespace::RemoteZellijState::Missing
            && latest.cached_zellij_state.as_deref() == Some("connecting")
            && cache_updated_within(&latest, 60)
        {
            zellij_state = codespace::RemoteZellijState::Connecting;
        }

        let updated_at = if zellij_state == codespace::RemoteZellijState::Connecting {
            latest.codespace_state_updated_at.clone()
        } else {
            Some(now_str())
        };
        let mut update = Session::update_many()
            .col_expr(
                Column::CachedCodespaceState,
                Expr::value(Some(remote.state.to_ascii_lowercase())),
            )
            .col_expr(
                Column::CachedCodespaceBranch,
                Expr::value(remote.branch.clone()),
            )
            .col_expr(
                Column::CachedZellijState,
                Expr::value(Some(zellij_state.as_str().to_string())),
            )
            .col_expr(
                Column::CodespaceStateUpdatedAt,
                Expr::value(updated_at.clone()),
            )
            .filter(Column::CopilotUuid.eq(&session.copilot_uuid));
        update = match latest.codespace_state_updated_at.as_deref() {
            Some(value) => update.filter(Column::CodespaceStateUpdatedAt.eq(value)),
            None => update.filter(Column::CodespaceStateUpdatedAt.is_null()),
        };
        update = match latest.cached_codespace_state.as_deref() {
            Some(value) => update.filter(Column::CachedCodespaceState.eq(value)),
            None => update.filter(Column::CachedCodespaceState.is_null()),
        };
        update = match latest.cached_codespace_branch.as_deref() {
            Some(value) => update.filter(Column::CachedCodespaceBranch.eq(value)),
            None => update.filter(Column::CachedCodespaceBranch.is_null()),
        };
        update = match latest.cached_zellij_state.as_deref() {
            Some(value) => update.filter(Column::CachedZellijState.eq(value)),
            None => update.filter(Column::CachedZellijState.is_null()),
        };
        let result = update.exec(db).await?;
        if result.rows_affected == 0 {
            let current = Session::find()
                .filter(Column::CopilotUuid.eq(&session.copilot_uuid))
                .one(db)
                .await?
                .with_context(|| {
                    format!(
                        "Session with UUID '{}' disappeared during refresh",
                        session.copilot_uuid
                    )
                })?;
            use_session_cache(&mut states, details.name, current);
            continue;
        }

        states.values.insert(details.name.to_string(), remote);
        states
            .zellij_values
            .insert(details.name.to_string(), zellij_state);
    }
    states.current_login = Some(current_login);
    Ok(states)
}

fn session_repo_label(session: &session::Model) -> Result<String> {
    let repo = git::repo_name(&session.source_repo);
    match session.backend.as_str() {
        BACKEND_LOCAL => Ok(repo),
        BACKEND_CODESPACE => Ok(format!("{repo}@cs")),
        backend => bail!("Session '{}' has unknown backend '{backend}'", session.name),
    }
}

fn session_display_branch(session: &session::Model, states: &CodespaceStates) -> Result<String> {
    match session.backend.as_str() {
        BACKEND_LOCAL => {
            if session.status == STATUS_REMOVED {
                Ok(session.branch.clone())
            } else {
                Ok(git::current_branch(&session.worktree_path)
                    .unwrap_or_else(|| session.branch.clone()))
            }
        }
        BACKEND_CODESPACE => {
            let details = codespace_details(session)?;
            if let Some(current_login) = states.current_login.as_deref()
                && current_login != details.github_login
            {
                return Ok(session.branch.clone());
            }
            Ok(states
                .values
                .get(details.name)
                .and_then(|state| state.branch.clone())
                .unwrap_or_else(|| session.branch.clone()))
        }
        backend => bail!("Session '{}' has unknown backend '{backend}'", session.name),
    }
}

fn session_display_status(
    session: &session::Model,
    zellij_state: &zellij::State,
    codespace_states: &CodespaceStates,
) -> Result<String> {
    if session.status == STATUS_REMOVED {
        return Ok(STATUS_REMOVED.to_string());
    }

    let zellij_name = zellij_session_name(session);
    match session.backend.as_str() {
        BACKEND_LOCAL => Ok(zellij_state.display_status(&zellij_name).to_string()),
        BACKEND_CODESPACE => {
            let details = codespace_details(session)?;
            let observed_zellij = codespace_states.zellij_values.get(details.name);
            if let Some(current_login) = codespace_states.current_login.as_deref()
                && current_login != details.github_login
            {
                let zellij_status = match observed_zellij {
                    Some(codespace::RemoteZellijState::Connecting)
                    | Some(codespace::RemoteZellijState::Running) => "running",
                    Some(codespace::RemoteZellijState::Exited) => "exited",
                    Some(codespace::RemoteZellijState::Missing) | None => "unknown",
                };
                return Ok(format!("{zellij_status}/account:{}", details.github_login));
            }
            let remote_status = codespace_states
                .values
                .get(details.name)
                .map(|state| state.state.to_ascii_lowercase())
                .unwrap_or_else(|| "unknown".to_string());
            let zellij_status = match observed_zellij {
                Some(codespace::RemoteZellijState::Connecting)
                | Some(codespace::RemoteZellijState::Running) => "running",
                Some(codespace::RemoteZellijState::Exited) => "exited",
                Some(codespace::RemoteZellijState::Missing) => "stopped",
                None => match remote_status.as_str() {
                    "shutdown" | "missing" => "stopped",
                    _ => "unknown",
                },
            };
            Ok(format!("{zellij_status}/{remote_status}"))
        }
        backend => bail!("Session '{}' has unknown backend '{backend}'", session.name),
    }
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
    let codespace_states = cached_codespace_states(&sessions, &zs);
    let mut entries: Vec<(&session::Model, String)> = sessions
        .iter()
        .map(|session| {
            Ok((
                session,
                session_display_status(session, &zs, &codespace_states)?,
            ))
        })
        .collect::<Result<_>>()?;

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

    entries
        .iter()
        .enumerate()
        .map(|(i, (s, status))| -> Result<interactive::Item> {
            // Use the same colored renderer as `csm ls`. The picker handles
            // embedded ANSI escapes when truncating/padding, and strips them
            // off the cursor row so reverse-video highlighting stays clean.
            let shortcode = display::format_shortcode(&hex_ids[i], unique_lens[i], true);
            let repo = session_repo_label(s)?;
            let branch = session_display_branch(s, &codespace_states)?;
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
            Ok(interactive::Item {
                key: s.name.clone(),
                display: display_line,
                search_text,
                hidden: s.status == STATUS_REMOVED,
            })
        })
        .collect()
}

pub async fn list(show_all: bool, refresh: bool) -> Result<()> {
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
    let codespace_states = if refresh {
        info!("Refreshing Codespace status cache");
        refresh_codespace_states(&db, &sessions, &zs).await?
    } else {
        cached_codespace_states(&sessions, &zs)
    };

    let mut entries: Vec<(&session::Model, String)> = sessions
        .iter()
        .map(|session| {
            Ok((
                session,
                session_display_status(session, &zs, &codespace_states)?,
            ))
        })
        .collect::<Result<_>>()?;

    entries.sort_by(|(a, sa), (b, sb)| {
        display::status_rank(sa.as_str())
            .cmp(&display::status_rank(sb.as_str()))
            .then(b.last_used_at.cmp(&a.last_used_at))
    });

    let hex_ids: Vec<String> = entries
        .iter()
        .map(|(s, _)| display::uuid_hex(&s.copilot_uuid))
        .collect();
    let unique_lens = display::shortest_unique_prefixes_within(&hex_ids, &all_hex_ids);

    for (i, (s, status)) in entries.iter().enumerate() {
        let shortcode = display::format_shortcode(&hex_ids[i], unique_lens[i], color);
        let repo = session_repo_label(s)?;
        let branch = session_display_branch(s, &codespace_states)?;
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
    record_session_span(&session);
    let sname = session.name.clone();
    let zname = zellij_session_name(&session);

    if session.status != STATUS_REMOVED {
        bail!(
            "Session '{sname}' is not removed (status: {}). Use `csm attach` instead.",
            session.status
        );
    }

    let uuid = session.copilot_uuid.clone();
    match session.backend.as_str() {
        BACKEND_LOCAL => {
            if !git::branch_exists(&session.branch, Some(&session.source_repo)) {
                bail!("Branch '{}' no longer exists", session.branch);
            }
            git::create_worktree(
                &session.worktree_path,
                &session.branch,
                false,
                Some(&session.source_repo),
            )?;
            let worktree = session.worktree_path.clone();

            let mut active: ActiveModel = session.into();
            active.status = Set(STATUS_ACTIVE.to_string());
            active.last_used_at = Set(now_str());
            active.update(&db).await?;

            info!(
                session.name = sname,
                session.uuid = uuid,
                session.backend = BACKEND_LOCAL,
                "Restored session"
            );
            let include_git = git::is_git_repo(&worktree);
            start_local_zellij_session(&db, &sname, &zname, &uuid, &worktree, true, include_git)
                .await
        }
        BACKEND_CODESPACE => {
            let details = codespace_details(&session)?;
            let codespace_name = details.name.to_string();
            let remote_workdir = details.workdir.to_string();
            let github_login = details.github_login.to_string();
            let initial_state = codespace::current_state(&codespace_name, &github_login)?;
            let setup_result = (|| -> Result<()> {
                let launcher = zellij::ensure_codespace_launcher()?;
                let layout = zellij::ensure_codespace_layout(&uuid, &codespace_name)?;
                let config = zellij::ensure_config()?;
                codespace::prepare_remote(codespace::RemoteSetup {
                    name: &codespace_name,
                    workdir: &remote_workdir,
                    launcher: &launcher,
                    layout: &layout,
                    config: &config,
                    uuid: &uuid,
                    resume: true,
                    github_login: &github_login,
                })
            })();
            if let Err(error) = setup_result {
                if initial_state.eq_ignore_ascii_case("shutdown")
                    && let Err(stop_error) =
                        stop_codespace_and_cache(&db, &codespace_name, &github_login, &uuid).await
                {
                    warn!(
                        session.name = sname,
                        session.uuid = uuid,
                        codespace.name = codespace_name,
                        error = %stop_error,
                        "Failed to stop Codespace after restore setup failed"
                    );
                }
                return Err(error);
            }

            let mut active: ActiveModel = session.clone().into();
            active.status = Set(STATUS_ACTIVE.to_string());
            active.last_used_at = Set(now_str());
            if let Err(error) = active.update(&db).await {
                if initial_state.eq_ignore_ascii_case("shutdown")
                    && let Err(stop_error) =
                        stop_codespace_and_cache(&db, &codespace_name, &github_login, &uuid).await
                {
                    warn!(
                        session.name = sname,
                        session.uuid = uuid,
                        codespace.name = codespace_name,
                        error = %stop_error,
                        "Failed to stop Codespace after restore database update failed"
                    );
                }
                return Err(error.into());
            }

            info!(
                session.name = sname,
                session.uuid = uuid,
                session.backend = BACKEND_CODESPACE,
                codespace.name = codespace_name,
                "Restored remote Zellij session"
            );
            match enter_codespace_zellij(&db, &session, false).await {
                Ok(()) => Ok(()),
                Err(error) => {
                    if let Err(mark_error) = mark_session_removed(&db, &uuid).await {
                        warn!(
                            session.name = sname,
                            session.uuid = uuid,
                            error = %mark_error,
                            "Failed to return session to removed state after connection failed"
                        );
                    }
                    if let Err(stop_error) =
                        stop_codespace_and_cache(&db, &codespace_name, &github_login, &uuid).await
                    {
                        warn!(
                            session.name = sname,
                            session.uuid = uuid,
                            codespace.name = codespace_name,
                            error = %stop_error,
                            "Failed to stop Codespace after restore connection failed"
                        );
                    }
                    Err(error)
                }
            }
        }
        backend => bail!("Session '{sname}' has unknown backend '{backend}'"),
    }
}

pub async fn rename(old: &str, new_name: &str) -> Result<()> {
    validate_name(new_name)?;
    let db = crate::db::connect().await?;
    let session = resolve_session(&db, old).await?;
    record_session_span(&session);
    let old_name = session.name.clone();
    let uuid = session.copilot_uuid.clone();
    let zname = zellij_session_name(&session);

    if old_name == new_name {
        bail!("New name is the same as the old name");
    }

    let txn = db.begin().await?;
    if Session::find_by_id(new_name).one(&txn).await?.is_some() {
        txn.rollback().await?;
        bail!("Session '{new_name}' already exists");
    }

    let session = Session::find()
        .filter(Column::CopilotUuid.eq(&uuid))
        .one(&txn)
        .await?
        .context("Session disappeared during rename")?;
    if session.name != old_name {
        txn.rollback().await?;
        bail!("Session '{old_name}' was renamed concurrently");
    }

    let new_session = ActiveModel {
        name: Set(new_name.to_string()),
        branch: Set(session.branch.clone()),
        copilot_uuid: Set(session.copilot_uuid.clone()),
        source_repo: Set(session.source_repo.clone()),
        worktree_path: Set(session.worktree_path.clone()),
        backend: Set(session.backend.clone()),
        codespace_name: Set(session.codespace_name.clone()),
        remote_workdir: Set(session.remote_workdir.clone()),
        github_login: Set(session.github_login.clone()),
        cached_codespace_state: Set(session.cached_codespace_state.clone()),
        cached_codespace_branch: Set(session.cached_codespace_branch.clone()),
        cached_zellij_state: Set(session.cached_zellij_state.clone()),
        codespace_state_updated_at: Set(session.codespace_state_updated_at.clone()),
        status: Set(session.status.clone()),
        last_used_at: Set(now_str()),
    };
    let deleted = Session::delete_many()
        .filter(Column::Name.eq(&old_name))
        .filter(Column::CopilotUuid.eq(&uuid))
        .exec(&txn)
        .await?;
    if deleted.rows_affected != 1 {
        txn.rollback().await?;
        bail!("Session '{old_name}' changed during rename");
    }
    new_session.insert(&txn).await?;
    txn.commit().await?;

    let suffix = match session.backend.as_str() {
        BACKEND_LOCAL if zellij::State::query().is_running(&zname) => " (still running)",
        BACKEND_LOCAL => "",
        BACKEND_CODESPACE => {
            if session
                .cached_codespace_state
                .as_deref()
                .is_some_and(|state| state.eq_ignore_ascii_case("available"))
            {
                " (Codespace available)"
            } else {
                ""
            }
        }
        backend => bail!("Session '{new_name}' has unknown backend '{backend}'"),
    };
    info!(
        target: "csm::result",
        session_old_name = old_name,
        session_name = new_name,
        session_uuid = uuid,
        details = suffix,
        "Renamed session"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached_codespace_session() -> session::Model {
        session::Model {
            name: "example".to_string(),
            branch: "main".to_string(),
            copilot_uuid: "abcdef01-2345-6789-abcd-ef0123456789".to_string(),
            source_repo: "octo/repo".to_string(),
            worktree_path: String::new(),
            backend: BACKEND_CODESPACE.to_string(),
            codespace_name: Some("studious-space-123".to_string()),
            remote_workdir: Some("/workspaces/repo".to_string()),
            github_login: Some("octocat".to_string()),
            cached_codespace_state: Some("available".to_string()),
            cached_codespace_branch: Some("feature".to_string()),
            cached_zellij_state: Some("running".to_string()),
            codespace_state_updated_at: Some("2026-07-28 14:00:00".to_string()),
            status: STATUS_ACTIVE.to_string(),
            last_used_at: "2026-07-28 14:00:00".to_string(),
        }
    }

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
    fn cached_codespace_status_needs_no_remote_query() {
        let session = cached_codespace_session();
        let sessions = vec![session.clone()];
        let local_zellij = zellij::State::from_sessions(Vec::new());
        let states = cached_codespace_states(&sessions, &local_zellij);

        assert_eq!(
            session_display_status(&session, &local_zellij, &states).unwrap(),
            "running/available"
        );
        assert_eq!(
            session_display_branch(&session, &states).unwrap(),
            "feature"
        );
        assert_eq!(states.current_login, None);
    }

    #[test]
    fn uncached_codespace_status_is_unknown() {
        let mut session = cached_codespace_session();
        session.cached_codespace_state = None;
        session.cached_zellij_state = None;
        let sessions = vec![session.clone()];
        let local_zellij = zellij::State::from_sessions(Vec::new());
        let states = cached_codespace_states(&sessions, &local_zellij);

        assert_eq!(
            session_display_status(&session, &local_zellij, &states).unwrap(),
            "unknown/unknown"
        );
    }

    #[test]
    fn recent_connecting_codespace_displays_as_running() {
        let mut session = cached_codespace_session();
        session.cached_zellij_state = Some("connecting".to_string());
        session.codespace_state_updated_at = Some(now_str());
        let sessions = vec![session.clone()];
        let local_zellij = zellij::State::from_sessions(Vec::new());
        let states = cached_codespace_states(&sessions, &local_zellij);

        assert!(cache_updated_within(&session, 60));
        assert_eq!(
            session_display_status(&session, &local_zellij, &states).unwrap(),
            "running/available"
        );
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
