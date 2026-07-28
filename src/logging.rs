use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result};
use tracing::{Level, debug};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::{LevelFilter, Targets, filter_fn};
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::prelude::*;

pub struct LoggingGuard {
    _file_guard: WorkerGuard,
}

pub fn debug_log_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not determine home directory")?;
    Ok(home.join(".csm").join("csm-debug.log"))
}

pub fn init() -> Result<LoggingGuard> {
    let path = debug_log_path()?;
    let directory = path
        .parent()
        .context("Debug log path does not have a parent directory")?;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("Failed to create {}", directory.display()))?;

    let file_name = path
        .file_name()
        .context("Debug log path does not have a file name")?;
    let file_appender = tracing_appender::rolling::never(directory, file_name);
    let (file_writer, file_guard) = tracing_appender::non_blocking(file_appender);
    let console_ansi = std::io::stderr().is_terminal();
    let console_result_filter = filter_fn(|metadata| {
        metadata.target() == "csm::result" && *metadata.level() == Level::INFO
    });
    let console_diagnostic_filter = filter_fn(|metadata| {
        (metadata.target().starts_with("csm")
            && metadata.target() != "csm::result"
            && *metadata.level() == Level::INFO)
            || matches!(*metadata.level(), Level::WARN | Level::ERROR)
    });
    let file_filter = Targets::new()
        .with_target("csm", LevelFilter::DEBUG)
        .with_default(LevelFilter::WARN);

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .compact()
                .without_time()
                .with_target(false)
                .with_ansi(console_ansi)
                .with_writer(std::io::stdout)
                .with_filter(console_result_filter),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .compact()
                .without_time()
                .with_target(false)
                .with_ansi(console_ansi)
                .with_writer(std::io::stderr)
                .with_filter(console_diagnostic_filter),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_target(true)
                .with_thread_ids(true)
                .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
                .with_writer(file_writer)
                .with_filter(file_filter),
        )
        .try_init()
        .context("Failed to initialize logging")?;

    debug!(
        log.path = %path.display(),
        process.id = std::process::id(),
        version = env!("CARGO_PKG_VERSION"),
        "Persistent debug logging initialized"
    );
    Ok(LoggingGuard {
        _file_guard: file_guard,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_log_has_clear_name() {
        assert_eq!(
            debug_log_path().unwrap().file_name().unwrap(),
            "csm-debug.log"
        );
    }
}
