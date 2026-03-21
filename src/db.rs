use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rusqlite::Connection;
use tokio::sync::Mutex;
use tracing::{debug, info};

/// Current schema version. Bump this when adding new migrations.
const SCHEMA_VERSION: u32 = 1;

/// Wrapper around a SQLite connection providing async access to the bot's
/// persistent storage.
#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

impl Database {
    /// Open (or create) the database at `path` and run any pending migrations.
    pub async fn open(path: &Path) -> Result<Self> {
        let path = path.to_owned();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection> {
            let conn = Connection::open(&path)
                .with_context(|| format!("Failed to open database at {}", path.display()))?;

            // Basic pragmas for robustness.
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "foreign_keys", "ON")?;
            conn.pragma_update(None, "busy_timeout", 5000)?;

            Ok(conn)
        })
        .await
        .context("Database open task panicked")??;

        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };

        db.run_migrations().await?;

        Ok(db)
    }

    /// Open an in-memory database for testing.
    #[cfg(test)]
    pub async fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("Failed to open in-memory database")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };

        db.run_migrations().await?;

        Ok(db)
    }

    /// Run pending database migrations.
    async fn run_migrations(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            migrate(&conn)
        })
        .await
        .context("Migration task panicked")?
    }
}

/// Run all migrations up to [`SCHEMA_VERSION`].
fn migrate(conn: &Connection) -> Result<()> {
    // Ensure the metadata table exists.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_meta (
             key   TEXT PRIMARY KEY,
             value TEXT NOT NULL
         );",
    )
    .context("Failed to create schema_meta table")?;

    let current: u32 = conn
        .query_row(
            "SELECT value FROM schema_meta WHERE key = 'version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    if current >= SCHEMA_VERSION {
        debug!("Database schema is up-to-date (version {})", SCHEMA_VERSION);
        return Ok(());
    }

    info!(
        "Migrating database from version {} to {}",
        current, SCHEMA_VERSION
    );

    // Version 1
    if current < 1 {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS key_sharing_rooms (
                 room_id    TEXT PRIMARY KEY,
                 enabled_at TEXT NOT NULL DEFAULT (datetime('now'))
             );",
        )
        .context("Migration v1: failed to create key_sharing_rooms")?;
    }

    conn.execute(
        "INSERT OR REPLACE INTO schema_meta (key, value) VALUES ('version', ?1)",
        [SCHEMA_VERSION.to_string()],
    )
    .context("Failed to update schema version")?;

    info!(
        "Database migration complete (now at version {})",
        SCHEMA_VERSION
    );
    Ok(())
}

impl Database {
    /// Mark a room as opted-in for automatic room key distribution.
    pub async fn enable_key_sharing(&self, room_id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let room_id = room_id.to_owned();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT OR IGNORE INTO key_sharing_rooms (room_id) VALUES (?1)",
                [&room_id],
            )
            .context("Failed to enable key sharing for room")?;
            Ok(())
        })
        .await
        .context("enable_key_sharing task panicked")?
    }

    /// Remove a room from the key-sharing opt-in list.
    pub async fn disable_key_sharing(&self, room_id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let room_id = room_id.to_owned();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "DELETE FROM key_sharing_rooms WHERE room_id = ?1",
                [&room_id],
            )
            .context("Failed to disable key sharing for room")?;
            Ok(())
        })
        .await
        .context("disable_key_sharing task panicked")?
    }

    /// Check whether a room has opted in to automatic key sharing.
    pub async fn is_key_sharing_enabled(&self, room_id: &str) -> Result<bool> {
        let conn = self.conn.clone();
        let room_id = room_id.to_owned();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM key_sharing_rooms WHERE room_id = ?1)",
                    [&room_id],
                    |row| row.get(0),
                )
                .context("Failed to query key sharing status")?;
            Ok(exists)
        })
        .await
        .context("is_key_sharing_enabled task panicked")?
    }

    /// Return all room IDs that have key sharing enabled.
    pub async fn list_key_sharing_rooms(&self) -> Result<Vec<String>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare("SELECT room_id FROM key_sharing_rooms ORDER BY room_id")
                .context("Failed to prepare key_sharing_rooms query")?;
            let rows = stmt
                .query_map([], |row| row.get(0))
                .context("Failed to query key_sharing_rooms")?;
            let mut rooms = Vec::new();
            for row in rows {
                rooms.push(row.context("Failed to read room_id row")?);
            }
            Ok(rooms)
        })
        .await
        .context("list_key_sharing_rooms task panicked")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_open_in_memory() {
        let db = Database::open_in_memory().await.unwrap();
        // Should be able to query without error.
        let rooms = db.list_key_sharing_rooms().await.unwrap();
        assert!(rooms.is_empty());
    }

    #[tokio::test]
    async fn test_enable_disable_key_sharing() {
        let db = Database::open_in_memory().await.unwrap();
        let room = "!test:example.com";

        assert!(!db.is_key_sharing_enabled(room).await.unwrap());

        db.enable_key_sharing(room).await.unwrap();
        assert!(db.is_key_sharing_enabled(room).await.unwrap());

        // Enabling again should be idempotent.
        db.enable_key_sharing(room).await.unwrap();
        assert!(db.is_key_sharing_enabled(room).await.unwrap());

        db.disable_key_sharing(room).await.unwrap();
        assert!(!db.is_key_sharing_enabled(room).await.unwrap());

        // Disabling again should be idempotent.
        db.disable_key_sharing(room).await.unwrap();
        assert!(!db.is_key_sharing_enabled(room).await.unwrap());
    }

    #[tokio::test]
    async fn test_list_key_sharing_rooms() {
        let db = Database::open_in_memory().await.unwrap();

        db.enable_key_sharing("!b:example.com").await.unwrap();
        db.enable_key_sharing("!a:example.com").await.unwrap();
        db.enable_key_sharing("!c:example.com").await.unwrap();

        let rooms = db.list_key_sharing_rooms().await.unwrap();
        assert_eq!(
            rooms,
            vec!["!a:example.com", "!b:example.com", "!c:example.com"]
        );
    }

    #[tokio::test]
    async fn test_migration_is_idempotent() {
        let db = Database::open_in_memory().await.unwrap();

        // Running migrations again should not fail.
        db.run_migrations().await.unwrap();
        db.run_migrations().await.unwrap();

        // DB should still work.
        db.enable_key_sharing("!room:example.com").await.unwrap();
        assert!(
            db.is_key_sharing_enabled("!room:example.com")
                .await
                .unwrap()
        );
    }
}
