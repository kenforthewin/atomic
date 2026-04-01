//! Database manager for multi-database support.
//!
//! `DatabaseManager` holds the registry and a lazy-loaded map of `AtomicCore`
//! instances. It provides the main entry point for server and desktop code
//! to resolve which database to operate on.

use crate::error::AtomicCoreError;
use crate::registry::{DatabaseInfo, Registry};
use crate::AtomicCore;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Manages multiple knowledge-base databases with a shared registry.
pub struct DatabaseManager {
    registry: RwLock<Option<Arc<Registry>>>,
    cores: RwLock<HashMap<String, AtomicCore>>,
    active_id: RwLock<String>,
    /// Optional passphrase for SQLCipher encryption (SQLite only).
    passphrase: RwLock<Option<String>>,
    /// Stored so deferred initialization can create databases later.
    data_dir: PathBuf,
    /// Postgres connection URL, if using Postgres backend.
    /// Stored so `get_core` can create new lightweight cores for different db_ids.
    #[cfg(feature = "postgres")]
    database_url: Option<String>,
}

impl DatabaseManager {
    /// Create a new manager, opening or creating the registry in `data_dir` (unencrypted).
    pub fn new(data_dir: impl AsRef<Path>) -> Result<Self, AtomicCoreError> {
        Self::new_encrypted(data_dir, None)
    }

    /// Create a new manager with optional SQLCipher encryption.
    pub fn new_encrypted(
        data_dir: impl AsRef<Path>,
        passphrase: Option<String>,
    ) -> Result<Self, AtomicCoreError> {
        let data_dir = data_dir.as_ref().to_path_buf();
        let registry = Arc::new(Registry::open_or_create_encrypted(
            &data_dir,
            passphrase.clone(),
        )?);
        let default_id = registry.get_default_database_id()?;

        Ok(DatabaseManager {
            registry: RwLock::new(Some(registry)),
            cores: RwLock::new(HashMap::new()),
            active_id: RwLock::new(default_id),
            passphrase: RwLock::new(passphrase),
            data_dir,
            #[cfg(feature = "postgres")]
            database_url: None,
        })
    }

    /// Create a manager in deferred mode — no databases are created yet.
    /// Call `initialize()` later (e.g. from the setup/claim endpoint) to create them.
    pub fn new_deferred(data_dir: impl AsRef<Path>) -> Self {
        DatabaseManager {
            registry: RwLock::new(None),
            cores: RwLock::new(HashMap::new()),
            active_id: RwLock::new("default".to_string()),
            passphrase: RwLock::new(None),
            data_dir: data_dir.as_ref().to_path_buf(),
            #[cfg(feature = "postgres")]
            database_url: None,
        }
    }

    /// Returns true if the manager has been initialized (databases exist).
    pub fn is_initialized(&self) -> bool {
        self.registry.read().map(|r| r.is_some()).unwrap_or(false)
    }

    /// Get the data directory path.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Initialize the manager, creating the registry and databases with an optional passphrase.
    /// Called during the setup/claim flow when databases were deferred.
    ///
    /// Uses the registry write lock to prevent two concurrent requests from both
    /// initializing with potentially mismatched passphrases (TOCTOU guard).
    pub fn initialize(&self, passphrase: Option<String>) -> Result<(), AtomicCoreError> {
        // Acquire the registry write lock FIRST, then check under the lock to
        // prevent a race where two concurrent requests both see is_initialized() == false.
        let mut reg = self.registry.write().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        if reg.is_some() {
            return Err(AtomicCoreError::Configuration(
                "Manager already initialized".to_string(),
            ));
        }

        let registry = Arc::new(Registry::open_or_create_encrypted(
            &self.data_dir,
            passphrase.clone(),
        )?);
        let default_id = registry.get_default_database_id()?;

        // Store passphrase
        {
            let mut pp = self.passphrase.write().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            *pp = passphrase;
        }

        // Store registry (already holding the write lock)
        *reg = Some(registry);

        // Update active ID
        {
            let mut active = self.active_id.write().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            *active = default_id;
        }

        Ok(())
    }

    /// Get the registry, returning an error if not yet initialized.
    fn require_registry(&self) -> Result<Arc<Registry>, AtomicCoreError> {
        let reg = self.registry.read().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        reg.clone().ok_or_else(|| AtomicCoreError::Configuration(
            "Server not initialized — complete setup first".to_string(),
        ))
    }

    /// Create a manager that uses Postgres for data storage.
    /// In Postgres mode, there is no separate registry for database management —
    /// the `databases` table lives in Postgres. Settings, tokens, and DB metadata
    /// all go through Postgres storage. The SQLite registry is still created for
    /// OAuth routes but database CRUD uses the Postgres `databases` table.
    #[cfg(feature = "postgres")]
    pub fn new_postgres(
        data_dir: impl AsRef<Path>,
        database_url: &str,
    ) -> Result<Self, AtomicCoreError> {
        // Still need a registry for the DatabaseManager struct (used by OAuth routes).
        // But AtomicCore gets registry: None so all settings/tokens fall through to Postgres storage.
        let registry = Arc::new(Registry::open_or_create(&data_dir)?);

        // Use a temporary db_id to bootstrap; we'll look up the real default from Postgres.
        let core = AtomicCore::open_postgres(
            database_url,
            "default",
            None, // No registry — everything goes through Postgres
        )?;

        // Ensure the default database entry exists in Postgres
        let databases = core.storage.list_databases_sync()?;
        if databases.is_empty() {
            // No databases at all — seed the default entry
            let now = chrono::Utc::now().to_rfc3339();
            // Use raw SQL to set is_default = 1 (create_database_sync sets 0)
            if let Some(pg) = core.storage.as_postgres() {
                crate::storage::pg_runtime_block_on(async {
                    sqlx::query(
                        "INSERT INTO databases (id, name, is_default, created_at) VALUES ($1, $2, 1, $3)
                         ON CONFLICT (id) DO NOTHING",
                    )
                    .bind("default")
                    .bind("Default")
                    .bind(&now)
                    .execute(&pg.pool)
                    .await
                    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))
                })?;
            }
        }

        let default_id = core.storage.get_default_database_id_sync()?;

        // If the core was bootstrapped with a different db_id, recreate with the real one
        let core = if default_id != "default" {
            AtomicCore::open_postgres(database_url, &default_id, None)?
        } else {
            core
        };

        let mut cores_map = HashMap::new();
        cores_map.insert(default_id.clone(), core);

        Ok(DatabaseManager {
            registry: RwLock::new(Some(registry)),
            cores: RwLock::new(cores_map),
            active_id: RwLock::new(default_id),
            passphrase: RwLock::new(None),
            data_dir: data_dir.as_ref().to_path_buf(),
            #[cfg(feature = "postgres")]
            database_url: Some(database_url.to_string()),
        })
    }

    /// Returns true if this manager is using Postgres storage.
    #[cfg(feature = "postgres")]
    fn is_postgres(&self) -> bool {
        self.database_url.is_some()
    }

    /// Helper: get a storage backend to call database management methods.
    /// In Postgres mode, grabs the storage from any loaded core (they all share a pool).
    #[cfg(feature = "postgres")]
    fn any_storage(&self) -> Result<crate::storage::StorageBackend, AtomicCoreError> {
        let cores = self.cores.read().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        cores
            .values()
            .next()
            .map(|c| c.storage.clone())
            .ok_or_else(|| AtomicCoreError::Configuration("No cores loaded".to_string()))
    }

    /// Resolve a database identifier to its canonical ID.
    /// If the value matches an existing database ID, returns it as-is.
    /// Otherwise, tries a case-insensitive name lookup.
    fn resolve_database_id(&self, id_or_name: &str) -> Result<String, AtomicCoreError> {
        #[cfg(feature = "postgres")]
        if self.is_postgres() {
            let databases = self.any_storage()?.list_databases_sync()?;
            if databases.iter().any(|d| d.id == id_or_name) {
                return Ok(id_or_name.to_string());
            }
            if let Some(db) = databases.iter().find(|d| d.name.eq_ignore_ascii_case(id_or_name)) {
                return Ok(db.id.clone());
            }
            return Err(AtomicCoreError::NotFound(format!("Database '{}'", id_or_name)));
        }

        // SQLite path: check registry
        let databases = self.require_registry()?.list_databases()?;
        if databases.iter().any(|d| d.id == id_or_name) {
            return Ok(id_or_name.to_string());
        }
        if let Some(db) = self.require_registry()?.find_database_by_name(id_or_name)? {
            return Ok(db.id);
        }
        // Return the original value — let downstream handle not-found
        Ok(id_or_name.to_string())
    }

    /// Get a core for a specific database, loading it lazily if needed.
    /// Accepts either a database ID or name — if `id` doesn't match a known
    /// database ID, it falls back to a case-insensitive name lookup.
    pub fn get_core(&self, id: &str) -> Result<AtomicCore, AtomicCoreError> {
        // Fast path: already loaded by id
        {
            let cores = self.cores.read().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            if let Some(core) = cores.get(id) {
                return Ok(core.clone());
            }
        }

        // If the id doesn't look like a known database id, try resolving by name
        let resolved_id = self.resolve_database_id(id)?;
        if resolved_id != id {
            // Check cache again with the resolved id
            let cores = self.cores.read().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            if let Some(core) = cores.get(&resolved_id) {
                return Ok(core.clone());
            }
        }
        let id = &resolved_id;

        // Postgres path: create lightweight core sharing the same pool with a new db_id
        #[cfg(feature = "postgres")]
        if let Some(ref url) = self.database_url {
            // Get the pool from an existing core to share it
            let existing_core = {
                let cores = self.cores.read().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                cores.values().next().cloned()
            };
            if let Some(existing) = existing_core {
                if let Some(pg) = existing.storage.as_postgres() {
                    let new_pg = pg.with_db_id(id);
                    let core = AtomicCore::from_postgres_storage(new_pg);
                    // Seed default tags for this db_id if needed
                    let all_tags = core.storage.get_all_tags_impl()?;
                    if all_tags.is_empty() {
                        for category in &["Topics", "People", "Locations", "Organizations", "Events"] {
                            core.storage.create_tag_impl(category, None)?;
                        }
                    }
                    let mut cores = self.cores.write().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                    cores.insert(id.to_string(), core.clone());
                    if let Err(e) = self.require_registry().and_then(|r| r.touch_database(id)) {
                        tracing::warn!(db_id = %id, error = %e, "failed to touch database timestamp");
                    }
                    return Ok(core);
                }
            }
        }

        // SQLite path: load from disk
        let registry = self.require_registry()?;
        let db_path = registry.database_path(id);
        let pp = self.passphrase.read().map_err(|e| AtomicCoreError::Lock(e.to_string()))?.clone();
        let core = AtomicCore::open_for_server_encrypted(
            &db_path,
            pp,
            Some(registry),
        )?;

        self.require_registry()?.touch_database(id)?;

        let mut cores = self.cores.write().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        cores.insert(id.to_string(), core.clone());
        Ok(core)
    }

    /// Get the active (current) database core.
    pub fn active_core(&self) -> Result<AtomicCore, AtomicCoreError> {
        let id = self.active_id.read().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        self.get_core(&id)
    }

    /// Get the active database ID.
    pub fn active_id(&self) -> Result<String, AtomicCoreError> {
        let id = self.active_id.read().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        Ok(id.clone())
    }

    /// Switch the active database.
    pub fn set_active(&self, id: &str) -> Result<(), AtomicCoreError> {
        // Validate the database exists
        #[cfg(feature = "postgres")]
        if self.is_postgres() {
            let databases = self.any_storage()?.list_databases_sync()?;
            if !databases.iter().any(|d| d.id == id) {
                return Err(AtomicCoreError::NotFound(format!("Database '{}'", id)));
            }
        } else {
            let databases = self.require_registry()?.list_databases()?;
            if !databases.iter().any(|d| d.id == id) {
                return Err(AtomicCoreError::NotFound(format!("Database '{}'", id)));
            }
        }

        #[cfg(not(feature = "postgres"))]
        {
            let databases = self.require_registry()?.list_databases()?;
            if !databases.iter().any(|d| d.id == id) {
                return Err(AtomicCoreError::NotFound(format!("Database '{}'", id)));
            }
        }

        // Ensure it's loaded
        self.get_core(id)?;

        let mut active = self.active_id.write().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        *active = id.to_string();
        Ok(())
    }

    /// Get the registry for settings/token/database CRUD.
    /// Returns an error if the manager is not yet initialized (deferred mode).
    pub fn registry(&self) -> Result<Arc<Registry>, AtomicCoreError> {
        self.require_registry()
    }

    /// Create a new database and register it.
    pub fn create_database(&self, name: &str) -> Result<DatabaseInfo, AtomicCoreError> {
        #[cfg(feature = "postgres")]
        if self.is_postgres() {
            let storage = self.any_storage()?;
            let info = storage.create_database_sync(name)?;

            // Create a core for the new database (shares Postgres pool, new db_id)
            if let Some(pg) = storage.as_postgres() {
                let new_pg = pg.with_db_id(&info.id);
                let core = AtomicCore::from_postgres_storage(new_pg);
                // Seed default tags
                let all_tags = core.storage.get_all_tags_impl()?;
                if all_tags.is_empty() {
                    for category in &["Topics", "People", "Locations", "Organizations", "Events"] {
                        core.storage.create_tag_impl(category, None)?;
                    }
                }
                let mut cores = self.cores.write().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                cores.insert(info.id.clone(), core);
            }

            return Ok(info);
        }

        let registry = self.require_registry()?;
        let info = registry.create_database(name)?;

        // Create the actual SQLite file
        let db_path = registry.database_path(&info.id);
        let pp = self.passphrase.read().map_err(|e| AtomicCoreError::Lock(e.to_string()))?.clone();
        let core = AtomicCore::open_for_server_encrypted(
            &db_path,
            pp,
            Some(registry),
        )?;

        let mut cores = self.cores.write().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        cores.insert(info.id.clone(), core);

        Ok(info)
    }

    /// Delete a database (cannot delete default). Removes from cache and disk.
    pub fn delete_database(&self, id: &str) -> Result<(), AtomicCoreError> {
        #[cfg(feature = "postgres")]
        if self.is_postgres() {
            // Postgres storage validates it's not the default
            let storage = self.any_storage()?;
            storage.delete_database_sync(id)?;

            // Remove from cache
            {
                let mut cores =
                    self.cores.write().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                cores.remove(id);
            }

            // If this was the active database, switch to default
            {
                let active = self.active_id.read().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                if *active == id {
                    drop(active);
                    let default_id = storage.get_default_database_id_sync()?;
                    let mut active =
                        self.active_id.write().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                    *active = default_id;
                }
            }

            // Purge all per-database data rows for this db_id
            storage.purge_database_data_sync(id)?;
            return Ok(());
        }

        // SQLite path: Registry validates it's not the default
        self.require_registry()?.delete_database(id)?;

        // Remove from cache
        {
            let mut cores =
                self.cores.write().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            if let Some(core) = cores.remove(id) {
                core.optimize();
            }
        }

        // If this was the active database, switch to default
        {
            let active = self.active_id.read().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            if *active == id {
                drop(active);
                let default_id = self.require_registry()?.get_default_database_id()?;
                let mut active =
                    self.active_id.write().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                *active = default_id;
            }
        }

        // Delete the file
        let db_path = self.require_registry()?.database_path(id);
        if db_path.exists() {
            std::fs::remove_file(&db_path).ok();
            // Also remove WAL/SHM
            std::fs::remove_file(db_path.with_extension("db-wal")).ok();
            std::fs::remove_file(db_path.with_extension("db-shm")).ok();
        }

        Ok(())
    }

    /// List all databases with their info, plus which is active.
    pub fn list_databases(&self) -> Result<(Vec<DatabaseInfo>, String), AtomicCoreError> {
        #[cfg(feature = "postgres")]
        if self.is_postgres() {
            let databases = self.any_storage()?.list_databases_sync()?;
            let active = self.active_id.read().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            return Ok((databases, active.clone()));
        }

        let databases = self.require_registry()?.list_databases()?;
        let active = self.active_id.read().map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        Ok((databases, active.clone()))
    }

    /// Rename a database.
    pub fn rename_database(&self, id: &str, name: &str) -> Result<(), AtomicCoreError> {
        #[cfg(feature = "postgres")]
        if self.is_postgres() {
            return self.any_storage()?.rename_database_sync(id, name);
        }

        self.require_registry()?.rename_database(id, name)
    }

    /// Set a database as the new default.
    pub fn set_default_database(&self, id: &str) -> Result<(), AtomicCoreError> {
        #[cfg(feature = "postgres")]
        if self.is_postgres() {
            return self.any_storage()?.set_default_database_sync(id);
        }

        self.require_registry()?.set_default_database(id)
    }

    /// Optimize all loaded cores (call on shutdown).
    pub fn optimize_all(&self) {
        if let Ok(cores) = self.cores.read() {
            for core in cores.values() {
                core.optimize();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_new_manager() {
        let dir = TempDir::new().unwrap();
        let manager = DatabaseManager::new(dir.path()).unwrap();

        let (databases, active_id) = manager.list_databases().unwrap();
        assert_eq!(databases.len(), 1);
        assert_eq!(active_id, "default");
    }

    #[test]
    fn test_get_active_core() {
        let dir = TempDir::new().unwrap();
        let manager = DatabaseManager::new(dir.path()).unwrap();

        let core = manager.active_core().unwrap();
        // Should be able to query the core
        let settings = core.get_settings().unwrap();
        assert!(settings.contains_key("provider"));
    }

    #[test]
    fn test_create_and_switch_database() {
        let dir = TempDir::new().unwrap();
        let manager = DatabaseManager::new(dir.path()).unwrap();

        let info = manager.create_database("Work").unwrap();
        assert_eq!(info.name, "Work");

        manager.set_active(&info.id).unwrap();
        let active = manager.active_id().unwrap();
        assert_eq!(active, info.id);
    }

    #[test]
    fn test_delete_database() {
        let dir = TempDir::new().unwrap();
        let manager = DatabaseManager::new(dir.path()).unwrap();

        let info = manager.create_database("Temp").unwrap();
        manager.delete_database(&info.id).unwrap();

        let (databases, _) = manager.list_databases().unwrap();
        assert_eq!(databases.len(), 1); // only default
    }

    #[test]
    fn test_delete_active_switches_to_default() {
        let dir = TempDir::new().unwrap();
        let manager = DatabaseManager::new(dir.path()).unwrap();

        let info = manager.create_database("Temp").unwrap();
        manager.set_active(&info.id).unwrap();
        manager.delete_database(&info.id).unwrap();

        let active = manager.active_id().unwrap();
        assert_eq!(active, "default");
    }

    #[test]
    fn test_cannot_delete_default() {
        let dir = TempDir::new().unwrap();
        let manager = DatabaseManager::new(dir.path()).unwrap();

        let result = manager.delete_database("default");
        assert!(result.is_err());
    }
}
