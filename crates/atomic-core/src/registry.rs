//! Registry database for shared configuration across multiple knowledge bases.
//!
//! The registry holds settings, API tokens, OAuth data, and metadata about
//! available databases. It uses its own SQLite file (`registry.db`) with a
//! separate migration track from data databases.

use crate::error::AtomicCoreError;
use crate::tokens::{self, ApiTokenInfo};
use chrono::Utc;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use uuid::Uuid;

/// Metadata about a knowledge-base database.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DatabaseInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub created_at: String,
    pub last_opened_at: Option<String>,
}

/// Information about an OAuth authorization code.
#[derive(Debug, Clone)]
pub struct OAuthCodeInfo {
    pub client_id: String,
    pub code_challenge: String,
    pub expires_at: String,
    pub used: bool,
}

/// The registry manages shared configuration and database metadata.
/// Lives at `<data_dir>/registry.db`.
pub struct Registry {
    conn: Mutex<Connection>,
    data_dir: PathBuf,
    passphrase: Option<String>,
}

impl Registry {
    /// Open or create the registry, running legacy migration if needed.
    ///
    /// If `registry.db` doesn't exist but `atomic.db` does in the data dir,
    /// performs legacy migration (moves atomic.db to databases/default.db,
    /// copies settings/tokens into registry).
    /// Open or create the registry (unencrypted).
    pub fn open_or_create(data_dir: impl AsRef<Path>) -> Result<Self, AtomicCoreError> {
        Self::open_or_create_encrypted(data_dir, None)
    }

    /// Open or create the registry with optional encryption.
    pub fn open_or_create_encrypted(
        data_dir: impl AsRef<Path>,
        passphrase: Option<String>,
    ) -> Result<Self, AtomicCoreError> {
        let data_dir = data_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&data_dir)?;

        let registry_path = data_dir.join("registry.db");
        let legacy_path = data_dir.join("atomic.db");
        let databases_dir = data_dir.join("databases");

        let needs_legacy_migration = !registry_path.exists() && legacy_path.exists();
        let is_fresh_install = !registry_path.exists() && !legacy_path.exists();

        // Open/create registry.db
        let conn = Connection::open(&registry_path)?;
        // PRAGMA key must come first if encrypted
        if let Some(ref pp) = passphrase {
            conn.pragma_update(None, "key", pp)?;
        }
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; \
             PRAGMA synchronous=NORMAL; \
             PRAGMA busy_timeout=5000;",
        )?;

        Self::run_migrations(&conn)?;

        std::fs::create_dir_all(&databases_dir)?;

        if needs_legacy_migration {
            Self::migrate_legacy(&conn, &legacy_path, &databases_dir)?;
        } else if is_fresh_install {
            // Create default database entry — the actual .db file is created
            // lazily when DatabaseManager first opens it.
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "INSERT OR IGNORE INTO databases (id, name, is_default, created_at) VALUES (?1, ?2, 1, ?3)",
                rusqlite::params!["default", "Default", &now],
            )?;
        }

        Ok(Registry {
            conn: Mutex::new(conn),
            data_dir,
            passphrase,
        })
    }

    // ==================== Migrations ====================

    const LATEST_VERSION: i32 = 1;

    fn run_migrations(conn: &Connection) -> Result<(), AtomicCoreError> {
        let version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;

        if version < 1 {
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS databases (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    is_default INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL,
                    last_opened_at TEXT
                );

                CREATE TABLE IF NOT EXISTS settings (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS api_tokens (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    token_hash TEXT NOT NULL,
                    token_prefix TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    last_used_at TEXT,
                    is_revoked INTEGER DEFAULT 0
                );

                CREATE INDEX IF NOT EXISTS idx_api_tokens_hash ON api_tokens(token_hash);

                CREATE TABLE IF NOT EXISTS oauth_clients (
                    id TEXT PRIMARY KEY,
                    client_id TEXT UNIQUE NOT NULL,
                    client_secret_hash TEXT NOT NULL,
                    client_name TEXT NOT NULL,
                    redirect_uris TEXT NOT NULL,
                    created_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS oauth_codes (
                    code_hash TEXT PRIMARY KEY,
                    client_id TEXT NOT NULL,
                    code_challenge TEXT NOT NULL,
                    code_challenge_method TEXT NOT NULL DEFAULT 'S256',
                    redirect_uri TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    expires_at TEXT NOT NULL,
                    used INTEGER NOT NULL DEFAULT 0,
                    token_id TEXT
                );

                PRAGMA user_version = 1;
                "#,
            )?;

            // Seed default settings
            crate::settings::migrate_settings_to(conn)?;
        }

        Ok(())
    }

    // ==================== Legacy Migration ====================

    /// Migrate from single atomic.db to registry + databases/default.db layout.
    fn migrate_legacy(
        conn: &Connection,
        legacy_path: &Path,
        databases_dir: &Path,
    ) -> Result<(), AtomicCoreError> {
        let default_path = databases_dir.join("default.db");

        // Move atomic.db (+ WAL/SHM) to databases/default.db
        std::fs::rename(legacy_path, &default_path)?;
        // Move WAL and SHM if they exist (non-fatal if missing)
        let wal = legacy_path.with_extension("db-wal");
        let shm = legacy_path.with_extension("db-shm");
        if wal.exists() {
            std::fs::rename(&wal, databases_dir.join("default.db-wal")).ok();
        }
        if shm.exists() {
            std::fs::rename(&shm, databases_dir.join("default.db-shm")).ok();
        }

        // Open the moved database to copy shared tables
        let data_conn = Connection::open(&default_path)?;

        // Copy settings
        {
            let mut stmt = data_conn.prepare("SELECT key, value FROM settings")?;
            let rows: Vec<(String, String)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            for (key, value) in rows {
                conn.execute(
                    "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
                    rusqlite::params![&key, &value],
                )?;
            }
        }

        // Copy api_tokens
        {
            let mut stmt = data_conn.prepare(
                "SELECT id, name, token_hash, token_prefix, created_at, last_used_at, is_revoked FROM api_tokens",
            )?;
            let rows: Vec<(String, String, String, String, String, Option<String>, i32)> = stmt
                .query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            for (id, name, hash, prefix, created, last_used, revoked) in rows {
                conn.execute(
                    "INSERT OR IGNORE INTO api_tokens (id, name, token_hash, token_prefix, created_at, last_used_at, is_revoked)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![&id, &name, &hash, &prefix, &created, &last_used, revoked],
                )?;
            }
        }

        // Copy oauth_clients
        {
            let has_table: bool = data_conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name='oauth_clients'",
                    [],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            if has_table {
                let mut stmt = data_conn.prepare(
                    "SELECT id, client_id, client_secret_hash, client_name, redirect_uris, created_at FROM oauth_clients",
                )?;
                let rows: Vec<(String, String, String, String, String, String)> = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                for (id, client_id, hash, name, uris, created) in rows {
                    conn.execute(
                        "INSERT OR IGNORE INTO oauth_clients (id, client_id, client_secret_hash, client_name, redirect_uris, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        rusqlite::params![&id, &client_id, &hash, &name, &uris, &created],
                    )?;
                }
            }
        }

        // Copy oauth_codes
        {
            let has_table: bool = data_conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name='oauth_codes'",
                    [],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            if has_table {
                let mut stmt = data_conn.prepare(
                    "SELECT code_hash, client_id, code_challenge, code_challenge_method, redirect_uri, created_at, expires_at, used, token_id FROM oauth_codes",
                )?;
                let rows: Vec<(String, String, String, String, String, String, String, i32, Option<String>)> = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                            row.get(8)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                for (hash, client_id, challenge, method, uri, created, expires, used, token_id) in
                    rows
                {
                    conn.execute(
                        "INSERT OR IGNORE INTO oauth_codes (code_hash, client_id, code_challenge, code_challenge_method, redirect_uri, created_at, expires_at, used, token_id)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        rusqlite::params![
                            &hash, &client_id, &challenge, &method, &uri, &created, &expires,
                            used, &token_id
                        ],
                    )?;
                }
            }
        }

        // Insert databases row for the migrated default
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT OR IGNORE INTO databases (id, name, is_default, created_at, last_opened_at) VALUES (?1, ?2, 1, ?3, ?4)",
            rusqlite::params!["default", "Default", &now, &now],
        )?;

        tracing::info!("Legacy migration complete: atomic.db -> databases/default.db");
        Ok(())
    }

    // ==================== Database Management ====================

    /// List all registered databases.
    pub fn list_databases(&self) -> Result<Vec<DatabaseInfo>, AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let mut stmt = conn.prepare(
            "SELECT id, name, is_default, created_at, last_opened_at FROM databases ORDER BY is_default DESC, created_at ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(DatabaseInfo {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    is_default: row.get::<_, i32>(2)? != 0,
                    created_at: row.get(3)?,
                    last_opened_at: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Find a database by name (case-insensitive). Returns the first match.
    pub fn find_database_by_name(&self, name: &str) -> Result<Option<DatabaseInfo>, AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        match conn.query_row(
            "SELECT id, name, is_default, created_at, last_opened_at FROM databases WHERE name = ?1 COLLATE NOCASE LIMIT 1",
            [name],
            |row| {
                Ok(DatabaseInfo {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    is_default: row.get::<_, i32>(2)? != 0,
                    created_at: row.get(3)?,
                    last_opened_at: row.get(4)?,
                })
            },
        ) {
            Ok(info) => Ok(Some(info)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Create a new database entry. Returns the new database info.
    /// The actual SQLite file is created when the DatabaseManager opens it.
    pub fn create_database(&self, name: &str) -> Result<DatabaseInfo, AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO databases (id, name, is_default, created_at) VALUES (?1, ?2, 0, ?3)",
            rusqlite::params![&id, name, &now],
        )?;
        Ok(DatabaseInfo {
            id,
            name: name.to_string(),
            is_default: false,
            created_at: now,
            last_opened_at: None,
        })
    }

    /// Rename a database.
    pub fn rename_database(&self, id: &str, name: &str) -> Result<(), AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let updated = conn.execute(
            "UPDATE databases SET name = ?1 WHERE id = ?2",
            rusqlite::params![name, id],
        )?;
        if updated == 0 {
            return Err(AtomicCoreError::NotFound(format!("Database '{}'", id)));
        }
        Ok(())
    }

    /// Delete a database entry. Cannot delete the default database.
    /// Does NOT delete the .db file — the caller (DatabaseManager) handles that.
    pub fn delete_database(&self, id: &str) -> Result<(), AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;

        let is_default: bool = conn
            .query_row(
                "SELECT is_default FROM databases WHERE id = ?1",
                [id],
                |row| row.get::<_, i32>(0).map(|v| v != 0),
            )
            .map_err(|_| AtomicCoreError::NotFound(format!("Database '{}'", id)))?;

        if is_default {
            return Err(AtomicCoreError::Validation(
                "Cannot delete the default database".to_string(),
            ));
        }

        conn.execute("DELETE FROM databases WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Set a database as the default, clearing the flag from the previous default.
    /// Cannot set a non-existent database as default.
    pub fn set_default_database(&self, id: &str) -> Result<(), AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;

        // Verify the database exists
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM databases WHERE id = ?1",
                [id],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !exists {
            return Err(AtomicCoreError::NotFound(format!("Database '{}'", id)));
        }

        let tx = conn.unchecked_transaction()?;
        tx.execute("UPDATE databases SET is_default = 0 WHERE is_default = 1", [])?;
        tx.execute(
            "UPDATE databases SET is_default = 1 WHERE id = ?1",
            [id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Get the ID of the default database.
    pub fn get_default_database_id(&self) -> Result<String, AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        conn.query_row(
            "SELECT id FROM databases WHERE is_default = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|_| {
            AtomicCoreError::Configuration("No default database configured".to_string())
        })
    }

    /// Get the file path for a database by ID.
    pub fn database_path(&self, id: &str) -> PathBuf {
        self.data_dir.join("databases").join(format!("{}.db", id))
    }

    /// Update the last_opened_at timestamp for a database.
    pub fn touch_database(&self, id: &str) -> Result<(), AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE databases SET last_opened_at = ?1 WHERE id = ?2",
            rusqlite::params![&now, id],
        )?;
        Ok(())
    }

    /// Get the data directory path.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Open a new connection to registry.db (for concurrent access in OAuth routes etc.)
    pub fn new_connection(&self) -> Result<Connection, AtomicCoreError> {
        let path = self.data_dir.join("registry.db");
        let conn = Connection::open(&path)?;
        if let Some(ref pp) = self.passphrase {
            conn.pragma_update(None, "key", pp)?;
        }
        Ok(conn)
    }

    // ==================== Settings (shared across databases) ====================

    /// Get all settings.
    pub fn get_all_settings(&self) -> Result<HashMap<String, String>, AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        crate::settings::get_all_settings(&conn)
    }

    /// Get a single setting by key.
    pub fn get_setting(&self, key: &str) -> Result<String, AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        crate::settings::get_setting(&conn, key)
    }

    /// Set a setting (upsert).
    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        crate::settings::set_setting(&conn, key, value)
    }

    // ==================== API Tokens (shared across databases) ====================

    /// Create a new named API token.
    pub fn create_api_token(
        &self,
        name: &str,
    ) -> Result<(ApiTokenInfo, String), AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        tokens::create_token(&conn, name)
    }

    /// List all API tokens (metadata only).
    pub fn list_api_tokens(&self) -> Result<Vec<ApiTokenInfo>, AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        tokens::list_tokens(&conn)
    }

    /// Verify a raw API token.
    pub fn verify_api_token(
        &self,
        raw_token: &str,
    ) -> Result<Option<ApiTokenInfo>, AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        tokens::verify_token(&conn, raw_token)
    }

    /// Revoke an API token by ID.
    pub fn revoke_api_token(&self, id: &str) -> Result<(), AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        tokens::revoke_token(&conn, id)
    }

    /// Update last_used_at for a token.
    pub fn update_token_last_used(&self, id: &str) -> Result<(), AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        tokens::update_last_used(&conn, id)
    }

    /// Migrate legacy server_auth_token.
    pub fn migrate_legacy_token(&self) -> Result<bool, AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        tokens::migrate_legacy_token(&conn)
    }

    /// Ensure at least one token exists.
    pub fn ensure_default_token(
        &self,
    ) -> Result<Option<(ApiTokenInfo, String)>, AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        tokens::ensure_default_token(&conn)
    }

    // ==================== OAuth (shared across databases) ====================

    /// Register a new OAuth client. Returns the generated client_id.
    pub fn create_oauth_client(
        &self,
        client_name: &str,
        client_secret_hash: &str,
        redirect_uris_json: &str,
    ) -> Result<String, AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        let id = Uuid::new_v4().to_string();
        let client_id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO oauth_clients (id, client_id, client_secret_hash, client_name, redirect_uris, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, client_id, client_secret_hash, client_name, redirect_uris_json, now],
        )?;
        Ok(client_id)
    }

    /// Get the client_name for an OAuth client by client_id.
    pub fn get_oauth_client_name(&self, client_id: &str) -> Result<Option<String>, AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        match conn.query_row(
            "SELECT client_name FROM oauth_clients WHERE client_id = ?1",
            [client_id],
            |row| row.get(0),
        ) {
            Ok(name) => Ok(Some(name)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(AtomicCoreError::Database(e)),
        }
    }

    /// Get the redirect_uris JSON for an OAuth client by client_id.
    pub fn get_oauth_client_redirect_uris(&self, client_id: &str) -> Result<Option<String>, AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        match conn.query_row(
            "SELECT redirect_uris FROM oauth_clients WHERE client_id = ?1",
            [client_id],
            |row| row.get(0),
        ) {
            Ok(uris) => Ok(Some(uris)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(AtomicCoreError::Database(e)),
        }
    }

    /// Get the client_secret_hash for an OAuth client by client_id.
    pub fn get_oauth_client_secret_hash(&self, client_id: &str) -> Result<Option<String>, AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        match conn.query_row(
            "SELECT client_secret_hash FROM oauth_clients WHERE client_id = ?1",
            [client_id],
            |row| row.get(0),
        ) {
            Ok(hash) => Ok(Some(hash)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(AtomicCoreError::Database(e)),
        }
    }

    /// Store a new OAuth authorization code.
    pub fn store_oauth_code(
        &self,
        code_hash: &str,
        client_id: &str,
        code_challenge: &str,
        code_challenge_method: &str,
        redirect_uri: &str,
        created_at: &str,
        expires_at: &str,
    ) -> Result<(), AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        conn.execute(
            "INSERT INTO oauth_codes (code_hash, client_id, code_challenge, code_challenge_method, redirect_uri, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![code_hash, client_id, code_challenge, code_challenge_method, redirect_uri, created_at, expires_at],
        )?;
        Ok(())
    }

    /// Look up an OAuth authorization code by its hash.
    /// Returns (client_id, code_challenge, expires_at, used).
    pub fn lookup_oauth_code(&self, code_hash: &str) -> Result<Option<OAuthCodeInfo>, AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        match conn.query_row(
            "SELECT client_id, code_challenge, expires_at, used FROM oauth_codes WHERE code_hash = ?1",
            [code_hash],
            |row| Ok(OAuthCodeInfo {
                client_id: row.get(0)?,
                code_challenge: row.get(1)?,
                expires_at: row.get(2)?,
                used: row.get::<_, i32>(3)? != 0,
            }),
        ) {
            Ok(info) => Ok(Some(info)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(AtomicCoreError::Database(e)),
        }
    }

    /// Mark an OAuth authorization code as used and optionally record the token_id.
    pub fn mark_oauth_code_used(&self, code_hash: &str, token_id: Option<&str>) -> Result<(), AtomicCoreError> {
        let conn = self.conn.lock().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        conn.execute(
            "UPDATE oauth_codes SET used = 1 WHERE code_hash = ?1",
            [code_hash],
        )?;
        if let Some(tid) = token_id {
            conn.execute(
                "UPDATE oauth_codes SET token_id = ?1 WHERE code_hash = ?2",
                rusqlite::params![tid, code_hash],
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_fresh_install() {
        let dir = TempDir::new().unwrap();
        let registry = Registry::open_or_create(dir.path()).unwrap();

        let databases = registry.list_databases().unwrap();
        assert_eq!(databases.len(), 1);
        assert_eq!(databases[0].id, "default");
        assert!(databases[0].is_default);
        assert_eq!(databases[0].name, "Default");
    }

    #[test]
    fn test_create_and_list_databases() {
        let dir = TempDir::new().unwrap();
        let registry = Registry::open_or_create(dir.path()).unwrap();

        registry.create_database("Work").unwrap();
        registry.create_database("Personal").unwrap();

        let databases = registry.list_databases().unwrap();
        assert_eq!(databases.len(), 3); // default + 2 new
        assert!(databases[0].is_default); // default first
    }

    #[test]
    fn test_rename_database() {
        let dir = TempDir::new().unwrap();
        let registry = Registry::open_or_create(dir.path()).unwrap();

        let db = registry.create_database("Old Name").unwrap();
        registry.rename_database(&db.id, "New Name").unwrap();

        let databases = registry.list_databases().unwrap();
        let renamed = databases.iter().find(|d| d.id == db.id).unwrap();
        assert_eq!(renamed.name, "New Name");
    }

    #[test]
    fn test_delete_database() {
        let dir = TempDir::new().unwrap();
        let registry = Registry::open_or_create(dir.path()).unwrap();

        let db = registry.create_database("Temp").unwrap();
        registry.delete_database(&db.id).unwrap();

        let databases = registry.list_databases().unwrap();
        assert_eq!(databases.len(), 1); // only default remains
    }

    #[test]
    fn test_cannot_delete_default() {
        let dir = TempDir::new().unwrap();
        let registry = Registry::open_or_create(dir.path()).unwrap();

        let result = registry.delete_database("default");
        assert!(result.is_err());
    }

    #[test]
    fn test_set_default_database() {
        let dir = TempDir::new().unwrap();
        let registry = Registry::open_or_create(dir.path()).unwrap();

        let db = registry.create_database("Second").unwrap();
        assert!(!db.is_default);

        registry.set_default_database(&db.id).unwrap();

        let databases = registry.list_databases().unwrap();
        let old_default = databases.iter().find(|d| d.id == "default").unwrap();
        let new_default = databases.iter().find(|d| d.id == db.id).unwrap();
        assert!(!old_default.is_default);
        assert!(new_default.is_default);

        // Verify get_default_database_id returns the new one
        let default_id = registry.get_default_database_id().unwrap();
        assert_eq!(default_id, db.id);
    }

    #[test]
    fn test_set_default_nonexistent() {
        let dir = TempDir::new().unwrap();
        let registry = Registry::open_or_create(dir.path()).unwrap();
        let result = registry.set_default_database("nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_settings_crud() {
        let dir = TempDir::new().unwrap();
        let registry = Registry::open_or_create(dir.path()).unwrap();

        // Default settings should be populated
        let settings = registry.get_all_settings().unwrap();
        assert!(settings.contains_key("provider"));

        // Set and get custom setting
        registry.set_setting("my_key", "my_value").unwrap();
        let value = registry.get_setting("my_key").unwrap();
        assert_eq!(value, "my_value");
    }

    #[test]
    fn test_token_crud() {
        let dir = TempDir::new().unwrap();
        let registry = Registry::open_or_create(dir.path()).unwrap();

        let (info, raw) = registry.create_api_token("test").unwrap();
        assert!(raw.starts_with("at_"));

        let verified = registry.verify_api_token(&raw).unwrap();
        assert!(verified.is_some());
        assert_eq!(verified.unwrap().id, info.id);

        registry.revoke_api_token(&info.id).unwrap();
        let verified = registry.verify_api_token(&raw).unwrap();
        assert!(verified.is_none());
    }

    #[test]
    fn test_legacy_migration() {
        let dir = TempDir::new().unwrap();
        let legacy_path = dir.path().join("atomic.db");

        // Create a legacy database with settings and tokens
        {
            let conn = Connection::open(&legacy_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO settings (key, value) VALUES ('provider', 'ollama');
                 INSERT INTO settings (key, value) VALUES ('chat_model', 'llama3');
                 CREATE TABLE api_tokens (
                     id TEXT PRIMARY KEY, name TEXT NOT NULL, token_hash TEXT NOT NULL,
                     token_prefix TEXT NOT NULL, created_at TEXT NOT NULL,
                     last_used_at TEXT, is_revoked INTEGER DEFAULT 0
                 );
                 INSERT INTO api_tokens (id, name, token_hash, token_prefix, created_at) VALUES ('t1', 'test', 'hash1', 'at_test', '2024-01-01');",
            )
            .unwrap();
        }

        // Open registry — should trigger legacy migration
        let registry = Registry::open_or_create(dir.path()).unwrap();

        // Legacy file should be moved
        assert!(!legacy_path.exists());
        assert!(dir.path().join("databases/default.db").exists());

        // Settings should be copied
        let settings = registry.get_all_settings().unwrap();
        assert_eq!(settings.get("provider").unwrap(), "ollama");
        assert_eq!(settings.get("chat_model").unwrap(), "llama3");

        // Tokens should be copied
        let tokens = registry.list_api_tokens().unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].name, "test");

        // Default database should be registered
        let databases = registry.list_databases().unwrap();
        assert_eq!(databases.len(), 1);
        assert_eq!(databases[0].id, "default");
        assert!(databases[0].is_default);
    }

    #[test]
    fn test_database_path() {
        let dir = TempDir::new().unwrap();
        let registry = Registry::open_or_create(dir.path()).unwrap();

        let path = registry.database_path("default");
        assert_eq!(path, dir.path().join("databases/default.db"));

        let path2 = registry.database_path("some-uuid");
        assert_eq!(path2, dir.path().join("databases/some-uuid.db"));
    }

    #[test]
    fn test_get_default_database_id() {
        let dir = TempDir::new().unwrap();
        let registry = Registry::open_or_create(dir.path()).unwrap();

        let id = registry.get_default_database_id().unwrap();
        assert_eq!(id, "default");
    }
}
