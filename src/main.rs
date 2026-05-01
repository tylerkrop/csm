use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;
mod db;
mod display;
mod entity;
mod git;
mod zellij;

#[derive(Parser)]
#[command(name = "csm", about = "Copilot Session Manager – manage Copilot sessions in Zellij")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create and start a new session
    #[command(alias = "r")]
    Run {
        /// Branch name suffix (full branch: tylerkrop/<name>)
        name: String,
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
    },
    /// List sessions (-a includes removed)
    #[command(alias = "ls", alias = "ps")]
    List {
        /// Show all sessions including removed
        #[arg(short, long)]
        all: bool,
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
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run { name } => commands::run(&name).await,
        Commands::Start { name } => commands::start(&name).await,
        Commands::Attach { name } => commands::attach(&name).await,
        Commands::Stop { names } => commands::stop(&names).await,
        Commands::Remove { names, force } => commands::rm(&names, force).await,
        Commands::List { all } => commands::list(all).await,
        Commands::Restore { name } => commands::restore(&name).await,
        Commands::Rename { old, new } => commands::rename(&old, &new).await,
    }
}
