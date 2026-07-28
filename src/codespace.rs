use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::{debug, info};

const MAX_DISPLAY_NAME_LEN: usize = 48;

#[derive(Debug, PartialEq, Eq)]
pub struct RepoInfo {
    pub name_with_owner: String,
    pub default_branch: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteState {
    pub state: String,
    pub branch: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteZellijState {
    Connecting,
    Running,
    Exited,
    Missing,
}

impl RemoteZellijState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Missing => "missing",
        }
    }

    pub fn from_cached(value: &str) -> Option<Self> {
        match value {
            "connecting" => Some(Self::Connecting),
            "running" => Some(Self::Running),
            "exited" => Some(Self::Exited),
            "missing" => Some(Self::Missing),
            _ => None,
        }
    }
}

pub struct RemoteSetup<'a> {
    pub name: &'a str,
    pub workdir: &'a str,
    pub launcher: &'a Path,
    pub layout: &'a Path,
    pub config: &'a Path,
    pub uuid: &'a str,
    pub resume: bool,
    pub github_login: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoView {
    name_with_owner: String,
    default_branch_ref: Option<BranchRef>,
}

#[derive(Deserialize)]
struct BranchRef {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodespaceListItem {
    name: String,
    state: String,
    git_status: Option<GitStatus>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodespaceIdentity {
    name: String,
    display_name: String,
}

#[derive(Deserialize)]
struct GitStatus {
    #[serde(rename = "ref")]
    branch: String,
}

fn checked_output(command: &mut Command, action: &str) -> Result<String> {
    debug!(action, command = ?command, "Running external command");
    let output = command
        .output()
        .with_context(|| format!("Failed to run {action}"))?;
    debug!(action, status = %output.status, "External command completed");
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            bail!("{action} failed with {}", output.status);
        }
        bail!("{action} failed: {stderr}");
    }
    String::from_utf8(output.stdout).with_context(|| format!("{action} returned invalid UTF-8"))
}

fn gh_output(args: &[&str], action: &str) -> Result<String> {
    checked_output(Command::new("gh").args(args), action)
}

fn gh_status(args: &[&str], action: &str) -> Result<()> {
    debug!(action, arguments = ?args, "Running interactive gh command");
    let status = Command::new("gh")
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("Failed to run {action}"))?;
    debug!(action, status = %status, "Interactive gh command completed");
    if !status.success() {
        bail!("{action} failed with {status}");
    }
    Ok(())
}

fn named_codespace_state(name: &str) -> Result<Option<String>> {
    validate_name(name)?;
    let endpoint = format!("user/codespaces/{name}");
    let output = Command::new("gh")
        .args([
            "api",
            "--hostname",
            "github.com",
            &endpoint,
            "--jq",
            ".state",
        ])
        .output()
        .context("Failed to query Codespace state")?;
    if output.status.success() {
        let state =
            String::from_utf8(output.stdout).context("Codespace state returned invalid UTF-8")?;
        let state = state.trim();
        if state.is_empty() {
            bail!("Codespace '{name}' returned an empty state");
        }
        return Ok(Some(state.to_string()));
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.contains("HTTP 404") {
        return Ok(None);
    }
    if stderr.is_empty() {
        bail!("Failed to query Codespace '{name}' with {}", output.status);
    }
    bail!("Failed to query Codespace '{name}': {stderr}");
}

fn parse_repo_info(stdout: &str) -> Result<RepoInfo> {
    let view: RepoView =
        serde_json::from_str(stdout).context("Failed to parse repository metadata from gh")?;
    let default_branch = view
        .default_branch_ref
        .map(|branch| branch.name)
        .context("The repository does not have a default branch")?;
    Ok(RepoInfo {
        name_with_owner: view.name_with_owner,
        default_branch,
    })
}

fn parse_login(stdout: &str) -> Result<String> {
    let login = stdout.trim();
    if login.is_empty()
        || !login
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        bail!("gh returned an invalid GitHub login");
    }
    Ok(login.to_string())
}

fn parse_states(stdout: &str) -> Result<HashMap<String, RemoteState>> {
    let items: Vec<CodespaceListItem> =
        serde_json::from_str(stdout).context("Failed to parse Codespace states from gh")?;
    Ok(items
        .into_iter()
        .map(|item| {
            (
                item.name,
                RemoteState {
                    state: item.state,
                    branch: item.git_status.map(|status| status.branch),
                },
            )
        })
        .collect())
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
        || !name
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric())
        || !name
            .chars()
            .last()
            .is_some_and(|character| character.is_ascii_alphanumeric())
    {
        bail!("Invalid Codespace name: {name}");
    }
    Ok(())
}

pub fn validate_remote_workdir(workdir: &str) -> Result<()> {
    let Some(repo_name) = workdir.strip_prefix("/workspaces/") else {
        bail!("Invalid Codespace workspace path: {workdir}");
    };
    if repo_name.is_empty()
        || !repo_name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
    {
        bail!("Invalid Codespace workspace path: {workdir}");
    }
    Ok(())
}

pub fn remote_workdir(repo: &str) -> Result<String> {
    let (_, repo_name) = repo
        .rsplit_once('/')
        .context("GitHub repository must be in owner/name form")?;
    let workdir = format!("/workspaces/{repo_name}");
    validate_remote_workdir(&workdir)?;
    Ok(workdir)
}

pub fn remote_launcher_path(codespace_name: &str) -> Result<String> {
    validate_name(codespace_name)?;
    Ok(format!("/tmp/csm-{codespace_name}-launch-codespace.sh"))
}

pub fn remote_layout_path(codespace_name: &str, uuid: &str) -> Result<String> {
    validate_name(codespace_name)?;
    uuid::Uuid::parse_str(uuid).with_context(|| format!("Invalid UUID: {uuid}"))?;
    Ok(format!(
        "/tmp/csm-{codespace_name}-{}.kdl",
        crate::display::short_uuid(uuid)
    ))
}

pub fn remote_config_path(codespace_name: &str) -> Result<String> {
    validate_name(codespace_name)?;
    Ok(format!("/tmp/csm-{codespace_name}-config.kdl"))
}

pub fn check_auth() -> Result<()> {
    gh_output(
        &["auth", "status", "--active", "--hostname", "github.com"],
        "gh auth status",
    )?;
    Ok(())
}

pub fn current_login() -> Result<String> {
    parse_login(&gh_output(
        &["api", "--hostname", "github.com", "user", "--jq", ".login"],
        "gh api user",
    )?)
}

fn ensure_account(expected_login: &str) -> Result<()> {
    let current_login = current_login()?;
    verify_account(&current_login, expected_login)
}

fn verify_account(current_login: &str, expected_login: &str) -> Result<()> {
    if current_login != expected_login {
        bail!(
            "Codespace belongs to GitHub account '{expected_login}', but gh is using \
             '{current_login}'. Run `gh auth switch --user {expected_login}` first."
        );
    }
    Ok(())
}

pub fn repo_info(repo_root: &str) -> Result<RepoInfo> {
    let mut command = Command::new("gh");
    command
        .args(["repo", "view", "--json", "nameWithOwner,defaultBranchRef"])
        .current_dir(repo_root);
    parse_repo_info(&checked_output(&mut command, "gh repo view")?)
}

fn codespace_display_name(session_name: &str, uuid: &str) -> String {
    let short_uuid = crate::display::short_uuid(uuid);
    format!("csm-{short_uuid}-{session_name}")
        .chars()
        .take(MAX_DISPLAY_NAME_LEN)
        .collect()
}

fn find_by_display_name(display_name: &str) -> Result<Option<String>> {
    let stdout = gh_output(
        &[
            "codespace",
            "list",
            "--limit",
            "1000",
            "--json",
            "name,displayName",
        ],
        "gh codespace list",
    )?;
    let matches: Vec<CodespaceIdentity> = serde_json::from_str::<Vec<CodespaceIdentity>>(&stdout)
        .context("Failed to parse Codespace identities from gh")?
        .into_iter()
        .filter(|codespace| codespace.display_name == display_name)
        .collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0].name.clone())),
        _ => bail!("Multiple Codespaces have display name '{display_name}'"),
    }
}

fn wait_for_created_codespace(display_name: &str) -> Result<Option<String>> {
    let mut last_error = None;
    for _ in 0..20 {
        match find_by_display_name(display_name) {
            Ok(Some(name)) => return Ok(Some(name)),
            Ok(None) => last_error = None,
            Err(error) => last_error = Some(error),
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    if let Some(error) = last_error {
        return Err(error);
    }
    Ok(None)
}

pub fn create(repo: &RepoInfo, session_name: &str, uuid: &str) -> Result<String> {
    let display_name = codespace_display_name(session_name, uuid);
    let status = Command::new("gh")
        .args([
            "codespace",
            "create",
            "--repo",
            &repo.name_with_owner,
            "--branch",
            &repo.default_branch,
            "--display-name",
            &display_name,
            "-s",
        ])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Failed to run gh codespace create")?;
    match wait_for_created_codespace(&display_name) {
        Ok(Some(name)) => {
            validate_name(&name)?;
            Ok(name)
        }
        Ok(None) if !status.success() => bail!("gh codespace create failed with {status}"),
        Ok(None) => bail!(
            "gh codespace create succeeded, but csm could not find display name \
             '{display_name}'. Locate it with `gh codespace list`."
        ),
        Err(error) => Err(error).with_context(|| {
            format!(
                "Could not resolve the Codespace named '{display_name}'. Locate it with \
                 `gh codespace list`."
            )
        }),
    }
}

fn file_sha256(path: &Path) -> Result<String> {
    let contents =
        std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(contents)))
}

fn parse_remote_file_hashes(stdout: &str, expected: usize) -> Result<Vec<Option<String>>> {
    let hashes: Vec<Option<String>> = stdout
        .lines()
        .map(|line| {
            let value = line.trim();
            if value == "missing" {
                Ok(None)
            } else if value.len() == 64
                && value.chars().all(|character| character.is_ascii_hexdigit())
            {
                Ok(Some(value.to_ascii_lowercase()))
            } else {
                bail!("Unexpected remote file checksum: {value}")
            }
        })
        .collect::<Result<_>>()?;
    if hashes.len() != expected {
        bail!(
            "Expected {expected} remote file checksums, received {}",
            hashes.len()
        );
    }
    Ok(hashes)
}

fn remote_file_hash_command(paths: &[&str]) -> Result<String> {
    if paths.is_empty() {
        bail!("No remote files supplied for checksum query");
    }
    for path in paths {
        if !path.starts_with("/tmp/csm-")
            || !path
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "/._-".contains(character))
        {
            bail!("Invalid remote checksum path: {path}");
        }
    }
    Ok(format!(
        r#"for path in {}; do if [ -f "$path" ]; then checksum="$(sha256sum "$path")" || exit; printf '%s\n' "${{checksum%% *}}"; else printf 'missing\n'; fi; done"#,
        paths.join(" ")
    ))
}

fn remote_file_hashes(
    name: &str,
    github_login: &str,
    paths: &[&str],
) -> Result<Vec<Option<String>>> {
    validate_name(name)?;
    ensure_account(github_login)?;
    let remote_command = remote_file_hash_command(paths)?;
    let mut command = Command::new("gh");
    command.args([
        "codespace",
        "ssh",
        "--codespace",
        name,
        "--",
        &remote_command,
    ]);
    parse_remote_file_hashes(
        &checked_output(&mut command, "Codespace Zellij file checksum query")?,
        paths.len(),
    )
}

pub fn prepare_remote(setup: RemoteSetup<'_>) -> Result<()> {
    let RemoteSetup {
        name,
        workdir,
        launcher,
        layout,
        config,
        uuid,
        resume,
        github_login,
    } = setup;
    validate_name(name)?;
    validate_remote_workdir(workdir)?;
    uuid::Uuid::parse_str(uuid).with_context(|| format!("Invalid UUID: {uuid}"))?;
    ensure_account(github_login)?;
    let launcher = launcher
        .to_str()
        .context("Codespace launcher path contains invalid UTF-8")?;
    let layout = layout
        .to_str()
        .context("Codespace layout path contains invalid UTF-8")?;
    let config = config
        .to_str()
        .context("Codespace config path contains invalid UTF-8")?;
    let remote_launcher = remote_launcher_path(name)?;
    let remote_layout = remote_layout_path(name, uuid)?;
    let remote_config = remote_config_path(name)?;
    let files = [
        (
            launcher,
            remote_launcher.as_str(),
            "Codespace launcher copy",
        ),
        (layout, remote_layout.as_str(), "Codespace layout copy"),
        (config, remote_config.as_str(), "Codespace config copy"),
    ];
    let remote_paths: Vec<&str> = files
        .iter()
        .map(|(_, destination, _)| *destination)
        .collect();
    let remote_hashes = remote_file_hashes(name, github_login, &remote_paths)?;
    let local_hashes = files
        .iter()
        .map(|(source, _, _)| file_sha256(Path::new(source)))
        .collect::<Result<Vec<_>>>()?;
    let changed: Vec<_> = files
        .iter()
        .zip(local_hashes.iter().zip(remote_hashes.iter()))
        .filter(|(_, (local, remote))| remote.as_ref() != Some(local))
        .map(|(file, _)| file)
        .collect();

    if changed.is_empty() {
        info!("Remote Zellij files are current");
    } else {
        info!(
            codespace.name = name,
            file.count = changed.len(),
            "Uploading changed remote Zellij files"
        );
    }
    for (source, destination, action) in changed {
        let remote_destination = format!("remote:{destination}");
        gh_output(
            &[
                "codespace",
                "cp",
                "--expand",
                "--codespace",
                name,
                source,
                &remote_destination,
            ],
            action,
        )?;
    }
    info!(
        codespace.name = name,
        session.uuid = uuid,
        "Checking Codespace workspace and dependencies"
    );
    let resume = if resume { "true" } else { "false" };
    gh_status(
        &[
            "codespace",
            "ssh",
            "--codespace",
            name,
            "--",
            "sh",
            &remote_launcher,
            "--check",
            workdir,
            &remote_layout,
            &remote_config,
            uuid,
            resume,
        ],
        "Codespace preflight",
    )?;
    info!(codespace.name = name, "Codespace environment is ready");
    Ok(())
}

fn remote_launcher_output(
    name: &str,
    github_login: &str,
    args: &[&str],
    action: &str,
) -> Result<String> {
    validate_name(name)?;
    ensure_account(github_login)?;
    let remote_launcher = remote_launcher_path(name)?;
    let mut command = Command::new("gh");
    command.args([
        "codespace",
        "ssh",
        "--codespace",
        name,
        "--",
        "sh",
        &remote_launcher,
    ]);
    command.args(args);
    checked_output(&mut command, action)
}

fn parse_remote_zellij_state(stdout: &str) -> Result<RemoteZellijState> {
    match stdout.trim() {
        "running" => Ok(RemoteZellijState::Running),
        "exited" => Ok(RemoteZellijState::Exited),
        "missing" => Ok(RemoteZellijState::Missing),
        value => bail!("Unexpected remote Zellij state: {value}"),
    }
}

pub fn remote_zellij_state(
    name: &str,
    uuid: &str,
    github_login: &str,
) -> Result<RemoteZellijState> {
    parse_remote_zellij_state(&remote_launcher_output(
        name,
        github_login,
        &["--state", uuid],
        "Remote Zellij state query",
    )?)
}

pub fn remote_zellij_ready(name: &str, uuid: &str, github_login: &str) -> Result<bool> {
    match remote_launcher_output(
        name,
        github_login,
        &["--ready", uuid],
        "Remote Zellij readiness query",
    )?
    .trim()
    {
        "ready" => Ok(true),
        "not-ready" => Ok(false),
        value => bail!("Unexpected remote Zellij readiness: {value}"),
    }
}

pub fn connect_zellij(
    name: &str,
    workdir: &str,
    uuid: &str,
    github_login: &str,
    attach_only: bool,
) -> Result<std::process::ExitStatus> {
    validate_name(name)?;
    validate_remote_workdir(workdir)?;
    uuid::Uuid::parse_str(uuid).with_context(|| format!("Invalid UUID: {uuid}"))?;
    ensure_account(github_login)?;
    let remote_launcher = remote_launcher_path(name)?;
    let remote_config = remote_config_path(name)?;
    remote_launcher_output(
        name,
        github_login,
        &["--clear-ready", uuid],
        "Remote Zellij readiness reset",
    )?;
    let mut command = Command::new("gh");
    command.args([
        "codespace",
        "ssh",
        "--codespace",
        name,
        "--",
        "-tt",
        "sh",
        &remote_launcher,
    ]);
    if attach_only {
        command.args(["--attach", uuid, workdir, &remote_config]);
    } else {
        let remote_layout = remote_layout_path(name, uuid)?;
        command.args(["--connect", uuid, workdir, &remote_layout, &remote_config]);
    }
    debug!(
        codespace.name = name,
        session.uuid = uuid,
        attach_only,
        command = ?command,
        "Connecting to remote Zellij"
    );
    let status = command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Failed to connect to remote Zellij")?;
    debug!(
        codespace.name = name,
        session.uuid = uuid,
        attach_only,
        %status,
        "Remote Zellij connection exited"
    );
    Ok(status)
}

pub fn cleanup_remote_zellij(name: &str, uuid: &str, github_login: &str) -> Result<()> {
    remote_launcher_output(
        name,
        github_login,
        &["--cleanup", uuid],
        "Remote Zellij cleanup",
    )?;
    Ok(())
}

pub fn stop(name: &str, github_login: &str) -> Result<()> {
    validate_name(name)?;
    ensure_account(github_login)?;
    let result = gh_output(
        &["codespace", "stop", "--codespace", name],
        "gh codespace stop",
    );
    if result.is_ok() {
        return Ok(());
    }

    match named_codespace_state(name)? {
        None => Ok(()),
        Some(state) if state.eq_ignore_ascii_case("shutdown") => Ok(()),
        Some(_) => result.map(|_| ()),
    }
}

pub fn delete(name: &str, github_login: &str) -> Result<()> {
    validate_name(name)?;
    ensure_account(github_login)?;
    gh_output(
        &["codespace", "delete", "--codespace", name, "--force"],
        "gh codespace delete",
    )?;
    Ok(())
}

pub fn delete_if_exists(name: &str, github_login: &str) -> Result<()> {
    validate_name(name)?;
    ensure_account(github_login)?;
    let result = delete(name, github_login);
    if result.is_ok() {
        return Ok(());
    }
    match named_codespace_state(name)? {
        None => Ok(()),
        Some(_) => result,
    }
}

pub fn current_state(name: &str, github_login: &str) -> Result<String> {
    validate_name(name)?;
    ensure_account(github_login)?;
    named_codespace_state(name)?.with_context(|| format!("Codespace '{name}' no longer exists"))
}

pub fn list_states() -> Result<HashMap<String, RemoteState>> {
    parse_states(&gh_output(
        &[
            "codespace",
            "list",
            "--limit",
            "1000",
            "--json",
            "name,state,gitStatus",
        ],
        "gh codespace list",
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repository_default_branch() {
        let info =
            parse_repo_info(r#"{"nameWithOwner":"octo/repo","defaultBranchRef":{"name":"main"}}"#)
                .unwrap();
        assert_eq!(
            info,
            RepoInfo {
                name_with_owner: "octo/repo".to_string(),
                default_branch: "main".to_string(),
            }
        );
    }

    #[test]
    fn display_name_contains_uuid_and_stays_within_limit() {
        assert_eq!(
            codespace_display_name("example", "abcdef01-2345-6789-abcd-ef0123456789"),
            "csm-abcdef01-example"
        );
        assert_eq!(
            codespace_display_name(
                "a-very-long-session-name-that-exceeds-the-limit",
                "abcdef01-2345-6789-abcd-ef0123456789"
            )
            .chars()
            .count(),
            MAX_DISPLAY_NAME_LEN
        );
    }

    #[test]
    fn parses_safe_github_login() {
        assert_eq!(parse_login("octo-cat\n").unwrap(), "octo-cat");
        assert!(parse_login("").is_err());
        assert!(parse_login("unsafe login").is_err());
    }

    #[test]
    fn verifies_codespace_account_owner() {
        assert!(verify_account("octocat", "octocat").is_ok());
        let error = verify_account("other", "octocat").unwrap_err().to_string();
        assert!(error.contains("gh auth switch --user octocat"));
    }

    #[test]
    fn remote_launcher_uses_existing_tmp_directory() {
        assert_eq!(
            remote_launcher_path("studious-space-123").unwrap(),
            "/tmp/csm-studious-space-123-launch-codespace.sh"
        );
        assert!(remote_launcher_path("unsafe name").is_err());
    }

    #[test]
    fn remote_zellij_paths_are_codespace_specific() {
        let uuid = "abcdef01-2345-6789-abcd-ef0123456789";
        assert_eq!(
            remote_layout_path("studious-space-123", uuid).unwrap(),
            "/tmp/csm-studious-space-123-abcdef01.kdl"
        );
        assert_eq!(
            remote_config_path("studious-space-123").unwrap(),
            "/tmp/csm-studious-space-123-config.kdl"
        );
    }

    #[test]
    fn parses_remote_zellij_states() {
        assert_eq!(
            parse_remote_zellij_state("running\n").unwrap(),
            RemoteZellijState::Running
        );
        assert_eq!(
            parse_remote_zellij_state("exited\n").unwrap(),
            RemoteZellijState::Exited
        );
        assert_eq!(
            parse_remote_zellij_state("missing\n").unwrap(),
            RemoteZellijState::Missing
        );
        assert!(parse_remote_zellij_state("unknown\n").is_err());
        assert_eq!(
            RemoteZellijState::from_cached(RemoteZellijState::Running.as_str()),
            Some(RemoteZellijState::Running)
        );
        assert_eq!(RemoteZellijState::from_cached("unknown"), None);
    }

    #[test]
    fn parses_remote_file_checksums() {
        let hash = "A".repeat(64);
        assert_eq!(
            parse_remote_file_hashes(&format!("{hash}\nmissing\n"), 2).unwrap(),
            vec![Some("a".repeat(64)), None]
        );
        assert!(parse_remote_file_hashes("invalid\n", 1).is_err());
        assert!(parse_remote_file_hashes("missing\n", 2).is_err());
    }

    #[test]
    fn builds_single_remote_checksum_command() {
        let command = remote_file_hash_command(&[
            "/tmp/csm-space-launch-codespace.sh",
            "/tmp/csm-space-abcdef01.kdl",
        ])
        .unwrap();
        assert!(command.starts_with("for path in /tmp/csm-space-launch-codespace.sh "));
        assert!(command.contains("sha256sum \"$path\""));
        assert!(command.ends_with("fi; done"));
        assert!(!command.contains("sh -c"));
        assert!(remote_file_hash_command(&["/tmp/csm-space;rm"]).is_err());
        assert!(remote_file_hash_command(&[]).is_err());
    }

    #[test]
    fn derives_safe_remote_workdir() {
        assert_eq!(
            remote_workdir("octo/my.repo_name-1").unwrap(),
            "/workspaces/my.repo_name-1"
        );
        assert!(remote_workdir("missing-owner").is_err());
        assert!(remote_workdir("octo/unsafe name").is_err());
        assert!(validate_remote_workdir("/tmp/repo").is_err());
    }

    #[test]
    fn parses_codespace_states() {
        let states = parse_states(
            r#"[{"name":"space-1","state":"Available","gitStatus":{"ref":"feature"}},
                {"name":"space-2","state":"Shutdown","gitStatus":null}]"#,
        )
        .unwrap();
        assert_eq!(
            states["space-1"],
            RemoteState {
                state: "Available".to_string(),
                branch: Some("feature".to_string()),
            }
        );
        assert_eq!(states["space-2"].branch, None);
    }
}
