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
        db_path.to_str().context("Database path contains invalid UTF-8")?
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

    Ok(db)
}
