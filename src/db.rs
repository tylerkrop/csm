use std::collections::HashSet;

use anyhow::{Context, Result};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Schema, Statement};

use crate::entity::session;

pub async fn connect() -> Result<DatabaseConnection> {
    let home = dirs::home_dir().context("Could not determine home directory")?;
    let db_dir = home.join(".csm");
    std::fs::create_dir_all(&db_dir)?;
    let db_path = db_dir.join("sessions.db");

    let url = format!(
        "sqlite:{}?mode=rwc",
        db_path
            .to_str()
            .context("Database path contains invalid UTF-8")?
    );
    let db = Database::connect(&url)
        .await
        .context("Failed to connect to database")?;

    let backend = db.get_database_backend();

    // Enable WAL mode for better concurrent reader/writer behavior, and set a
    // busy timeout so concurrent CSM invocations wait briefly for the lock
    // instead of immediately erroring with SQLITE_BUSY.
    for pragma in ["PRAGMA journal_mode=WAL;", "PRAGMA busy_timeout=5000;"] {
        db.execute(Statement::from_string(backend, pragma.to_string()))
            .await
            .with_context(|| format!("Failed to apply pragma: {pragma}"))?;
    }

    let schema = Schema::new(backend);
    let mut stmt = schema.create_table_from_entity(session::Entity);
    stmt.if_not_exists();
    db.execute(backend.build(&stmt))
        .await
        .context("Failed to create sessions table")?;

    ensure_session_columns(&db).await?;

    Ok(db)
}

async fn session_columns(db: &DatabaseConnection) -> Result<HashSet<String>> {
    let backend = db.get_database_backend();
    let rows = db
        .query_all(Statement::from_string(
            backend,
            "PRAGMA table_info('sessions')".to_string(),
        ))
        .await
        .context("Failed to inspect sessions table")?;
    rows.into_iter()
        .map(|row| {
            row.try_get("", "name")
                .context("Failed to read sessions table column")
        })
        .collect()
}

async fn ensure_session_columns(db: &DatabaseConnection) -> Result<()> {
    for (name, definition) in [
        ("backend", "TEXT NOT NULL DEFAULT 'local'"),
        ("codespace_name", "TEXT"),
        ("remote_workdir", "TEXT"),
        ("github_login", "TEXT"),
    ] {
        if session_columns(db).await?.contains(name) {
            continue;
        }

        let backend = db.get_database_backend();
        let statement = format!("ALTER TABLE sessions ADD COLUMN {name} {definition}");
        if let Err(error) = db.execute(Statement::from_string(backend, statement)).await
            && !session_columns(db).await?.contains(name)
        {
            return Err(error).with_context(|| format!("Failed to add sessions.{name}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrates_legacy_sessions_table() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let backend = db.get_database_backend();
        db.execute(Statement::from_string(
            backend,
            "CREATE TABLE sessions (
                name TEXT PRIMARY KEY NOT NULL,
                branch TEXT NOT NULL,
                copilot_uuid TEXT NOT NULL,
                source_repo TEXT NOT NULL,
                worktree_path TEXT NOT NULL,
                status TEXT NOT NULL,
                last_used_at TEXT NOT NULL
            );
            INSERT INTO sessions VALUES (
                'legacy', 'main', '11111111-1111-1111-1111-111111111111',
                '/tmp/repo', '/tmp/worktree', 'active', '2026-01-01 00:00:00'
            );"
            .to_string(),
        ))
        .await
        .unwrap();

        ensure_session_columns(&db).await.unwrap();

        let columns = session_columns(&db).await.unwrap();
        assert!(columns.contains("backend"));
        assert!(columns.contains("codespace_name"));
        assert!(columns.contains("remote_workdir"));
        assert!(columns.contains("github_login"));

        let row = db
            .query_one(Statement::from_string(
                backend,
                "SELECT backend, codespace_name, remote_workdir, github_login
                 FROM sessions WHERE name = 'legacy'"
                    .to_string(),
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<String>("", "backend").unwrap(), "local");
        assert_eq!(
            row.try_get::<Option<String>>("", "codespace_name").unwrap(),
            None
        );
        assert_eq!(
            row.try_get::<Option<String>>("", "remote_workdir").unwrap(),
            None
        );
        assert_eq!(
            row.try_get::<Option<String>>("", "github_login").unwrap(),
            None
        );
    }
}
