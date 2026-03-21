use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rusqlite::Connection;
use tokio::sync::Mutex;
use tracing::{debug, info};

/// Current schema version. Bump this when adding new migrations.
const SCHEMA_VERSION: u32 = 2;

/// Wrapper around a SQLite connection providing async access to the bot's
/// persistent storage.
#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone)]
pub struct CannedResponse {
    pub id: i64,
    pub text_markdown: Option<String>,
    pub media_cas_hash: Option<String>,
    pub media_mxc_uri: Option<String>,
    pub media_filename: Option<String>,
    pub media_mime_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CustomCommandRow {
    pub command_name: String,
    pub response: CannedResponse,
}

#[derive(Debug, Clone)]
pub struct AutoresponderRow {
    pub pattern: String,
    pub probability: f64,
    pub response: CannedResponse,
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

    // Version 2
    if current < 2 {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS canned_responses (
                 id              INTEGER PRIMARY KEY AUTOINCREMENT,
                 text_markdown   TEXT,
                 media_cas_hash  TEXT,
                 media_mxc_uri   TEXT,
                 media_filename  TEXT,
                 media_mime_type TEXT
             );
             CREATE TABLE IF NOT EXISTS custom_commands (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 room_id      TEXT NOT NULL,
                 command_name TEXT NOT NULL,
                 response_id  INTEGER NOT NULL REFERENCES canned_responses(id),
                 UNIQUE(room_id, command_name)
             );
             CREATE TABLE IF NOT EXISTS autoresponders (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 room_id     TEXT NOT NULL,
                 pattern     TEXT NOT NULL,
                 response_id INTEGER NOT NULL REFERENCES canned_responses(id),
                 probability REAL NOT NULL DEFAULT 1.0,
                 UNIQUE(room_id, pattern)
             );",
        )
        .context(
            "Migration v2: failed to create canned_responses/custom_commands/autoresponders",
        )?;
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

fn parse_canned_response(row: &rusqlite::Row) -> rusqlite::Result<CannedResponse> {
    Ok(CannedResponse {
        id: row.get(0)?,
        text_markdown: row.get(1)?,
        media_cas_hash: row.get(2)?,
        media_mxc_uri: row.get(3)?,
        media_filename: row.get(4)?,
        media_mime_type: row.get(5)?,
    })
}

impl Database {
    pub async fn create_canned_response(
        &self,
        text_markdown: Option<&str>,
        media_cas_hash: Option<&str>,
        media_filename: Option<&str>,
        media_mime_type: Option<&str>,
    ) -> Result<i64> {
        let conn = self.conn.clone();
        let text_markdown = text_markdown.map(|s| s.to_owned());
        let media_cas_hash = media_cas_hash.map(|s| s.to_owned());
        let media_filename = media_filename.map(|s| s.to_owned());
        let media_mime_type = media_mime_type.map(|s| s.to_owned());
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT INTO canned_responses (text_markdown, media_cas_hash, media_filename, media_mime_type)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![text_markdown, media_cas_hash, media_filename, media_mime_type],
            )
            .context("Failed to insert canned response")?;
            Ok(conn.last_insert_rowid())
        })
        .await
        .context("create_canned_response task panicked")?
    }

    pub async fn add_custom_command(
        &self,
        room_id: &str,
        command_name: &str,
        response_id: i64,
    ) -> Result<()> {
        let conn = self.conn.clone();
        let room_id = room_id.to_owned();
        let command_name = command_name.to_owned();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let old_response_id: Option<i64> = conn
                .query_row(
                    "SELECT response_id FROM custom_commands WHERE room_id = ?1 AND command_name = ?2",
                    rusqlite::params![&room_id, &command_name],
                    |row| row.get(0),
                )
                .ok();
            conn.execute(
                "INSERT OR REPLACE INTO custom_commands (room_id, command_name, response_id) VALUES (?1, ?2, ?3)",
                rusqlite::params![&room_id, &command_name, response_id],
            )
            .context("Failed to insert custom command")?;
            if let Some(old_rid) = old_response_id {
                let _ = conn.execute("DELETE FROM canned_responses WHERE id = ?1", [old_rid]);
            }
            Ok(())
        })
        .await
        .context("add_custom_command task panicked")?
    }

    pub async fn remove_custom_command(&self, room_id: &str, command_name: &str) -> Result<bool> {
        let conn = self.conn.clone();
        let room_id = room_id.to_owned();
        let command_name = command_name.to_owned();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let response_id: Option<i64> = conn
                .query_row(
                    "SELECT response_id FROM custom_commands WHERE room_id = ?1 AND command_name = ?2",
                    rusqlite::params![&room_id, &command_name],
                    |row| row.get(0),
                )
                .ok();
            if let Some(rid) = response_id {
                conn.execute(
                    "DELETE FROM custom_commands WHERE room_id = ?1 AND command_name = ?2",
                    rusqlite::params![&room_id, &command_name],
                )
                .context("Failed to delete custom command")?;
                conn.execute("DELETE FROM canned_responses WHERE id = ?1", [rid])
                    .context("Failed to delete canned response")?;
                Ok(true)
            } else {
                Ok(false)
            }
        })
        .await
        .context("remove_custom_command task panicked")?
    }

    pub async fn get_custom_command(
        &self,
        room_id: &str,
        command_name: &str,
    ) -> Result<Option<CannedResponse>> {
        let conn = self.conn.clone();
        let room_id = room_id.to_owned();
        let command_name = command_name.to_owned();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let result = conn.query_row(
                "SELECT cr.id, cr.text_markdown, cr.media_cas_hash, cr.media_mxc_uri,
                        cr.media_filename, cr.media_mime_type
                 FROM custom_commands cc
                 JOIN canned_responses cr ON cc.response_id = cr.id
                 WHERE cc.room_id = ?1 AND cc.command_name = ?2",
                rusqlite::params![&room_id, &command_name],
                parse_canned_response,
            );
            match result {
                Ok(cr) => Ok(Some(cr)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e).context("Failed to query custom command"),
            }
        })
        .await
        .context("get_custom_command task panicked")?
    }

    pub async fn list_custom_commands(&self, room_id: &str) -> Result<Vec<CustomCommandRow>> {
        let conn = self.conn.clone();
        let room_id = room_id.to_owned();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT cc.command_name, cr.id, cr.text_markdown, cr.media_cas_hash,
                        cr.media_mxc_uri, cr.media_filename, cr.media_mime_type
                 FROM custom_commands cc
                 JOIN canned_responses cr ON cc.response_id = cr.id
                 WHERE cc.room_id = ?1
                 ORDER BY cc.command_name",
                )
                .context("Failed to prepare custom_commands query")?;
            let rows = stmt
                .query_map([&room_id], |row| {
                    Ok(CustomCommandRow {
                        command_name: row.get(0)?,
                        response: CannedResponse {
                            id: row.get(1)?,
                            text_markdown: row.get(2)?,
                            media_cas_hash: row.get(3)?,
                            media_mxc_uri: row.get(4)?,
                            media_filename: row.get(5)?,
                            media_mime_type: row.get(6)?,
                        },
                    })
                })
                .context("Failed to query custom_commands")?;
            let mut result = Vec::new();
            for row in rows {
                result.push(row.context("Failed to read custom command row")?);
            }
            Ok(result)
        })
        .await
        .context("list_custom_commands task panicked")?
    }

    pub async fn add_autoresponder(
        &self,
        room_id: &str,
        pattern: &str,
        probability: f64,
        response_id: i64,
    ) -> Result<()> {
        let conn = self.conn.clone();
        let room_id = room_id.to_owned();
        let pattern = pattern.to_owned();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let old_response_id: Option<i64> = conn
                .query_row(
                    "SELECT response_id FROM autoresponders WHERE room_id = ?1 AND pattern = ?2",
                    rusqlite::params![&room_id, &pattern],
                    |row| row.get(0),
                )
                .ok();
            conn.execute(
                "INSERT OR REPLACE INTO autoresponders (room_id, pattern, probability, response_id)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![&room_id, &pattern, probability, response_id],
            )
            .context("Failed to insert autoresponder")?;
            if let Some(old_rid) = old_response_id {
                let _ = conn.execute("DELETE FROM canned_responses WHERE id = ?1", [old_rid]);
            }
            Ok(())
        })
        .await
        .context("add_autoresponder task panicked")?
    }

    pub async fn remove_autoresponder(&self, room_id: &str, pattern: &str) -> Result<bool> {
        let conn = self.conn.clone();
        let room_id = room_id.to_owned();
        let pattern = pattern.to_owned();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let response_id: Option<i64> = conn
                .query_row(
                    "SELECT response_id FROM autoresponders WHERE room_id = ?1 AND pattern = ?2",
                    rusqlite::params![&room_id, &pattern],
                    |row| row.get(0),
                )
                .ok();
            if let Some(rid) = response_id {
                conn.execute(
                    "DELETE FROM autoresponders WHERE room_id = ?1 AND pattern = ?2",
                    rusqlite::params![&room_id, &pattern],
                )
                .context("Failed to delete autoresponder")?;
                conn.execute("DELETE FROM canned_responses WHERE id = ?1", [rid])
                    .context("Failed to delete canned response")?;
                Ok(true)
            } else {
                Ok(false)
            }
        })
        .await
        .context("remove_autoresponder task panicked")?
    }

    pub async fn get_autoresponders(&self, room_id: &str) -> Result<Vec<AutoresponderRow>> {
        let conn = self.conn.clone();
        let room_id = room_id.to_owned();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT a.pattern, a.probability, cr.id, cr.text_markdown, cr.media_cas_hash,
                        cr.media_mxc_uri, cr.media_filename, cr.media_mime_type
                 FROM autoresponders a
                 JOIN canned_responses cr ON a.response_id = cr.id
                 WHERE a.room_id = ?1
                 ORDER BY a.id",
                )
                .context("Failed to prepare autoresponders query")?;
            let rows = stmt
                .query_map([&room_id], |row| {
                    Ok(AutoresponderRow {
                        pattern: row.get(0)?,
                        probability: row.get(1)?,
                        response: CannedResponse {
                            id: row.get(2)?,
                            text_markdown: row.get(3)?,
                            media_cas_hash: row.get(4)?,
                            media_mxc_uri: row.get(5)?,
                            media_filename: row.get(6)?,
                            media_mime_type: row.get(7)?,
                        },
                    })
                })
                .context("Failed to query autoresponders")?;
            let mut result = Vec::new();
            for row in rows {
                result.push(row.context("Failed to read autoresponder row")?);
            }
            Ok(result)
        })
        .await
        .context("get_autoresponders task panicked")?
    }

    pub async fn update_media_mxc(&self, response_id: i64, mxc_uri: &str) -> Result<()> {
        let conn = self.conn.clone();
        let mxc_uri = mxc_uri.to_owned();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE canned_responses SET media_mxc_uri = ?1 WHERE id = ?2",
                rusqlite::params![mxc_uri, response_id],
            )
            .context("Failed to update media mxc URI")?;
            Ok(())
        })
        .await
        .context("update_media_mxc task panicked")?
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

    #[tokio::test]
    async fn test_custom_command_crud() {
        let db = Database::open_in_memory().await.unwrap();
        let room = "!test:example.com";

        assert!(db.list_custom_commands(room).await.unwrap().is_empty());
        assert!(
            db.get_custom_command(room, "!links")
                .await
                .unwrap()
                .is_none()
        );

        let rid = db
            .create_canned_response(Some("Here are links"), None, None, None)
            .await
            .unwrap();
        db.add_custom_command(room, "!links", rid).await.unwrap();

        let resp = db
            .get_custom_command(room, "!links")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resp.text_markdown.as_deref(), Some("Here are links"));

        let cmds = db.list_custom_commands(room).await.unwrap();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].command_name, "!links");

        assert!(
            db.get_custom_command("!other:example.com", "!links")
                .await
                .unwrap()
                .is_none()
        );

        assert!(db.remove_custom_command(room, "!links").await.unwrap());
        assert!(
            db.get_custom_command(room, "!links")
                .await
                .unwrap()
                .is_none()
        );
        assert!(!db.remove_custom_command(room, "!links").await.unwrap());
    }

    #[tokio::test]
    async fn test_custom_command_replace() {
        let db = Database::open_in_memory().await.unwrap();
        let room = "!test:example.com";

        let r1 = db
            .create_canned_response(Some("old"), None, None, None)
            .await
            .unwrap();
        db.add_custom_command(room, "!cmd", r1).await.unwrap();

        let r2 = db
            .create_canned_response(Some("new"), None, None, None)
            .await
            .unwrap();
        db.add_custom_command(room, "!cmd", r2).await.unwrap();

        let resp = db.get_custom_command(room, "!cmd").await.unwrap().unwrap();
        assert_eq!(resp.text_markdown.as_deref(), Some("new"));
    }

    #[tokio::test]
    async fn test_autoresponder_crud() {
        let db = Database::open_in_memory().await.unwrap();
        let room = "!test:example.com";

        assert!(db.get_autoresponders(room).await.unwrap().is_empty());

        let rid = db
            .create_canned_response(Some("lol"), None, None, None)
            .await
            .unwrap();
        db.add_autoresponder(room, "hello+", 0.5, rid)
            .await
            .unwrap();

        let autos = db.get_autoresponders(room).await.unwrap();
        assert_eq!(autos.len(), 1);
        assert_eq!(autos[0].pattern, "hello+");
        assert_eq!(autos[0].probability, 0.5);
        assert_eq!(autos[0].response.text_markdown.as_deref(), Some("lol"));

        assert!(db.remove_autoresponder(room, "hello+").await.unwrap());
        assert!(db.get_autoresponders(room).await.unwrap().is_empty());
        assert!(!db.remove_autoresponder(room, "hello+").await.unwrap());
    }

    #[tokio::test]
    async fn test_canned_response_with_media() {
        let db = Database::open_in_memory().await.unwrap();

        let rid = db
            .create_canned_response(
                Some("Check this out"),
                Some("abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234"),
                Some("image.png"),
                Some("image/png"),
            )
            .await
            .unwrap();

        db.add_custom_command("!room:example.com", "!pic", rid)
            .await
            .unwrap();

        let resp = db
            .get_custom_command("!room:example.com", "!pic")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resp.text_markdown.as_deref(), Some("Check this out"));
        assert_eq!(
            resp.media_cas_hash.as_deref(),
            Some("abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234")
        );
        assert_eq!(resp.media_filename.as_deref(), Some("image.png"));
        assert_eq!(resp.media_mime_type.as_deref(), Some("image/png"));
        assert!(resp.media_mxc_uri.is_none());

        db.update_media_mxc(resp.id, "mxc://example.com/abc123")
            .await
            .unwrap();
        let resp = db
            .get_custom_command("!room:example.com", "!pic")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            resp.media_mxc_uri.as_deref(),
            Some("mxc://example.com/abc123")
        );
    }
}
