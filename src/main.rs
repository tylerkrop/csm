use std::future::Future;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::Instrument;

mod codespace;
mod commands;
mod db;
mod display;
mod entity;
mod git;
mod interactive;
mod logging;
mod zellij;

#[derive(Parser)]
#[command(
    name = "csm",
    about = "Copilot Session Manager – manage Copilot sessions in Zellij"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create and start a new session
    #[command(alias = "r")]
    Run {
        /// Session name and local branch suffix (full branch: tylerkrop/<name>)
        name: String,
        /// Skip worktree creation and run copilot directly in the current
        /// directory (no branch/worktree). Useful for hobby projects.
        #[arg(long, conflicts_with = "codespace")]
        here: bool,
        /// Create the session in a GitHub Codespace from the default branch
        #[arg(long, visible_alias = "cs", conflicts_with = "here")]
        codespace: bool,
    },
    /// Start a stopped session and attach
    #[command(alias = "s")]
    Start {
        /// Session name or UUID shortcode
        name: String,
    },
    /// Attach to a running session
    #[command(alias = "a")]
    Attach {
        /// Session name or UUID shortcode
        name: String,
    },
    /// Stop a session (kill Zellij, keep worktree)
    #[command(alias = "k")]
    Stop {
        /// Session names or UUID shortcodes
        names: Vec<String>,
    },
    /// Remove a session and its worktree
    #[command(alias = "rm")]
    Remove {
        /// Session names or UUID shortcodes
        names: Vec<String>,
        /// Permanently destroy (not restorable)
        #[arg(short, long)]
        force: bool,
        /// Pick sessions to remove from an interactive list
        #[arg(short, long, conflicts_with = "names")]
        interactive: bool,
        /// Also remove all sessions inactive for at least this many days
        #[arg(long, value_name = "DAYS")]
        older_than: Option<u64>,
    },
    /// List sessions (-a includes removed)
    #[command(alias = "ls", alias = "ps")]
    List {
        /// Show all sessions including removed
        #[arg(short, long)]
        all: bool,
        /// Refresh cached Codespace and remote Zellij status before listing
        #[arg(long)]
        refresh: bool,
    },
    /// Restore a previously removed session
    Restore {
        /// Session name or UUID shortcode
        name: String,
    },
    /// Rename a session
    #[command(alias = "mv")]
    Rename {
        /// Current session name or UUID shortcode
        old: String,
        /// New session name
        new: String,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let logging = match logging::init() {
        Ok(logging) => logging,
        Err(error) => {
            eprintln!("Failed to initialize logging: {error:#}");
            return ExitCode::FAILURE;
        }
    };
    let result = run(cli).await;
    let exit_code = match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    };
    drop(logging);
    exit_code
}

async fn logged_command<F>(span: tracing::Span, future: F) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    async move {
        let result = future.await;
        match &result {
            Ok(()) => tracing::debug!("Command completed"),
            Err(error) => tracing::error!(error = %format!("{error:#}"), "Command failed"),
        }
        result
    }
    .instrument(span)
    .await
}

async fn run(cli: Cli) -> Result<()> {
    let process_id = std::process::id();
    match cli.command {
        Commands::Run {
            name,
            here,
            codespace,
        } => {
            let span = tracing::debug_span!(
                "command",
                process.id = process_id,
                command = "run",
                session.query = %name,
                session.name = tracing::field::Empty,
                session.uuid = tracing::field::Empty,
                session.backend = tracing::field::Empty,
                here,
                codespace
            );
            logged_command(span, commands::run(&name, here, codespace)).await
        }
        Commands::Start { name } => {
            let span = tracing::debug_span!(
                "command",
                process.id = process_id,
                command = "start",
                session.query = %name,
                session.name = tracing::field::Empty,
                session.uuid = tracing::field::Empty,
                session.backend = tracing::field::Empty
            );
            logged_command(span, commands::start(&name)).await
        }
        Commands::Attach { name } => {
            let span = tracing::debug_span!(
                "command",
                process.id = process_id,
                command = "attach",
                session.query = %name,
                session.name = tracing::field::Empty,
                session.uuid = tracing::field::Empty,
                session.backend = tracing::field::Empty
            );
            logged_command(span, commands::attach(&name)).await
        }
        Commands::Stop { names } => {
            let span = tracing::debug_span!(
                "command",
                process.id = process_id,
                command = "stop",
                session.queries = ?names,
                session.name = tracing::field::Empty,
                session.uuid = tracing::field::Empty,
                session.backend = tracing::field::Empty
            );
            logged_command(span, commands::stop(&names)).await
        }
        Commands::Remove {
            names,
            force,
            interactive,
            older_than,
        } => {
            let span = tracing::debug_span!(
                "command",
                process.id = process_id,
                command = "remove",
                session.queries = ?names,
                session.name = tracing::field::Empty,
                session.uuid = tracing::field::Empty,
                session.backend = tracing::field::Empty,
                force,
                interactive,
                older_than
            );
            logged_command(span, commands::rm(&names, force, interactive, older_than)).await
        }
        Commands::List { all, refresh } => {
            let span = tracing::debug_span!(
                "command",
                process.id = process_id,
                command = "list",
                all,
                refresh
            );
            logged_command(span, commands::list(all, refresh)).await
        }
        Commands::Restore { name } => {
            let span = tracing::debug_span!(
                "command",
                process.id = process_id,
                command = "restore",
                session.query = %name,
                session.name = tracing::field::Empty,
                session.uuid = tracing::field::Empty,
                session.backend = tracing::field::Empty
            );
            logged_command(span, commands::restore(&name)).await
        }
        Commands::Rename { old, new } => {
            let span = tracing::debug_span!(
                "command",
                process.id = process_id,
                command = "rename",
                session.query = %old,
                session.new_name = %new,
                session.name = tracing::field::Empty,
                session.uuid = tracing::field::Empty,
                session.backend = tracing::field::Empty
            );
            logged_command(span, commands::rename(&old, &new)).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_accepts_codespace_alias() {
        let cli = Cli::try_parse_from(["csm", "run", "example", "--cs"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Run {
                codespace: true,
                here: false,
                ..
            }
        ));
    }

    #[test]
    fn run_rejects_codespace_with_here() {
        assert!(Cli::try_parse_from(["csm", "run", "example", "--cs", "--here"]).is_err());
    }

    #[test]
    fn list_accepts_refresh() {
        let cli = Cli::try_parse_from(["csm", "list", "--refresh"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::List {
                all: false,
                refresh: true
            }
        ));
    }
}
