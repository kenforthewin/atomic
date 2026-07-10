//! Database manager for multi-database support.
//!
//! `DatabaseManager` holds the registry and a lazy-loaded map of `AtomicCore`
//! instances. It provides the main entry point for server and desktop code
//! to resolve which database to operate on.

use crate::error::AtomicCoreError;
use crate::registry::{DatabaseInfo, Registry};
use crate::AtomicCore;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

/// The top-level tag categories seeded into every new knowledge base. The
/// auto-tagger only extends these (they are the default auto-tag targets), so
/// the set is a product decision — one source of truth here. (The V10→V11
/// SQLite migration in `db.rs` names the same five to backfill existing DBs;
/// that migration is frozen and must keep this list in mind if it ever grows.)
pub const DEFAULT_TAG_CATEGORIES: &[&str] =
    &["Topics", "People", "Locations", "Organizations", "Events"];

/// Manages multiple knowledge-base databases.
///
/// SQLite mode owns a `Registry` (`registry.db`) for cross-database state:
/// settings, API tokens, OAuth, and the `databases` index. Postgres mode keeps
/// all of that inside the Postgres database itself (see migration 006_oauth.sql),
/// so `registry` is `None` and nothing is written to the local filesystem.
pub struct DatabaseManager {
    /// SQLite registry, if running in SQLite mode. `None` for Postgres deployments.
    registry: Option<Arc<Registry>>,
    cores: RwLock<HashMap<String, AtomicCore>>,
    active_id: RwLock<String>,
    /// Postgres connection URL, if using Postgres backend.
    /// Stored so `get_core` can create new lightweight cores for different db_ids.
    #[cfg(feature = "postgres")]
    database_url: Option<String>,
}

impl DatabaseManager {
    /// Create a new manager, opening or creating the registry in `data_dir`.
    pub fn new(data_dir: impl AsRef<Path>) -> Result<Self, AtomicCoreError> {
        let registry = Arc::new(Registry::open_or_create(&data_dir)?);
        let default_id = registry.get_default_database_id()?;

        Ok(DatabaseManager {
            registry: Some(registry),
            cores: RwLock::new(HashMap::new()),
            active_id: RwLock::new(default_id),
            #[cfg(feature = "postgres")]
            database_url: None,
        })
    }

    /// Create a manager that uses Postgres for storage with no SQLite dependency.
    ///
    /// `data_dir` is unused for storage but kept in the signature so callers
    /// (CLI, server bootstrap) can pass through the same flag for both backends.
    /// Settings, tokens, OAuth, and the `databases` index all live in Postgres.
    ///
    /// The connection pool is sized from the `ATOMIC_PG_*` environment
    /// variables (see [`crate::storage::PgPoolConfig::from_env`]); use
    /// [`new_postgres_with_pool`](Self::new_postgres_with_pool) to size it
    /// explicitly.
    #[cfg(feature = "postgres")]
    pub async fn new_postgres(
        _data_dir: impl AsRef<Path>,
        database_url: &str,
    ) -> Result<Self, AtomicCoreError> {
        Self::new_postgres_with_pool(
            _data_dir,
            database_url,
            crate::storage::PgPoolConfig::from_env(),
        )
        .await
    }

    /// Like [`new_postgres`](Self::new_postgres), but with an explicit pool
    /// configuration instead of the `ATOMIC_PG_*` environment defaults.
    ///
    /// Callers managing many managers in one process (one per database URL)
    /// can use this to bound each manager's pool so the sum stays within the
    /// Postgres server's `max_connections`. Every core resolved through this
    /// manager shares the one pool, so the bound covers the whole manager.
    #[cfg(feature = "postgres")]
    pub async fn new_postgres_with_pool(
        _data_dir: impl AsRef<Path>,
        database_url: &str,
        pool_config: crate::storage::PgPoolConfig,
    ) -> Result<Self, AtomicCoreError> {
        Self::new_postgres_with_pool_and_provider(_data_dir, database_url, pool_config, None).await
    }

    /// Like [`new_postgres_with_pool`](Self::new_postgres_with_pool), but
    /// with an explicit provider configuration. `Some(config)` puts every
    /// core resolved through this manager into explicit provider-config mode
    /// (see [`AtomicCore::open_postgres_with_pool_and_provider`]): providers
    /// are built from `config` and the settings tables are never consulted
    /// for provider config. All cores share one live config slot, so
    /// [`AtomicCore::update_provider_config`] on any of them rotates the
    /// whole manager. `None` is byte-identical to
    /// [`new_postgres_with_pool`](Self::new_postgres_with_pool).
    #[cfg(feature = "postgres")]
    pub async fn new_postgres_with_pool_and_provider(
        _data_dir: impl AsRef<Path>,
        database_url: &str,
        pool_config: crate::storage::PgPoolConfig,
        provider_config: Option<crate::providers::ProviderConfig>,
    ) -> Result<Self, AtomicCoreError> {
        // Bootstrap with a placeholder db_id; we'll look up the real default from Postgres
        // once the schema has been migrated.
        let core = AtomicCore::open_postgres_with_pool_and_provider(
            database_url,
            "default",
            None,
            pool_config.clone(),
            provider_config.clone(),
        )
        .await?;

        // Seed the default database row if the `databases` table is empty.
        let databases = core.storage.list_databases_sync().await?;
        if databases.is_empty() {
            let now = chrono::Utc::now().to_rfc3339();
            // Use raw SQL to set is_default = 1 (create_database_sync sets 0)
            if let Some(pg) = core.storage.as_postgres() {
                sqlx::query(
                    "INSERT INTO databases (id, name, is_default, created_at) VALUES ($1, $2, 1, $3)
                     ON CONFLICT (id) DO NOTHING",
                )
                .bind("default")
                .bind("Default")
                .bind(&now)
                .execute(&pg.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            }
        }

        let default_id = core.storage.get_default_database_id_sync().await?;

        // If the bootstrap db_id doesn't match the resolved default, swap to a core scoped to the right db_id.
        let core = if default_id != "default" {
            AtomicCore::open_postgres_with_pool_and_provider(
                database_url,
                &default_id,
                None,
                pool_config,
                provider_config,
            )
            .await?
        } else {
            core
        };

        let mut cores_map = HashMap::new();
        cores_map.insert(default_id.clone(), core);

        Ok(DatabaseManager {
            registry: None,
            cores: RwLock::new(cores_map),
            active_id: RwLock::new(default_id),
            database_url: Some(database_url.to_string()),
        })
    }

    /// Returns true if this manager is using Postgres storage.
    #[cfg(feature = "postgres")]
    pub fn is_postgres(&self) -> bool {
        self.database_url.is_some()
    }

    /// Migrate a local SQLite database file into this manager's Postgres
    /// backend as a brand-new logical database. See [`crate::migrate`] for
    /// what is copied, what is regenerated, and the failure semantics.
    ///
    /// Errors with `Configuration` when the manager runs on SQLite storage.
    #[cfg(feature = "postgres")]
    pub async fn migrate_sqlite_to_postgres<F, C>(
        &self,
        source_db: &Path,
        options: crate::migrate::MigrationOptions,
        on_event: F,
        is_cancelled: C,
    ) -> Result<crate::migrate::MigrationReport, AtomicCoreError>
    where
        F: Fn(crate::migrate::MigrationEvent) + Send + Sync,
        C: Fn() -> bool + Send + Sync,
    {
        if !self.is_postgres() {
            return Err(AtomicCoreError::Configuration(
                "SQLite migration import requires this server to run on Postgres storage"
                    .to_string(),
            ));
        }
        let storage = self.any_storage()?;
        let pg = storage.as_postgres().ok_or_else(|| {
            AtomicCoreError::Configuration("Postgres storage unavailable".to_string())
        })?;
        crate::migrate::migrate_sqlite_to_postgres(source_db, pg, options, on_event, is_cancelled)
            .await
    }

    /// SQLite-only paths assume a registry is present. Centralizing the unwrap
    /// keeps the panic message uniform if the invariant is ever violated.
    fn sqlite_registry(&self) -> &Arc<Registry> {
        self.registry
            .as_ref()
            .expect("SQLite-only code path invoked without a registry (Postgres mode bug)")
    }

    /// Helper: get any loaded core. In Postgres mode every core shares one
    /// pool and one provider-config slot, so any of them can serve as the
    /// template for database management calls and sibling construction.
    #[cfg(feature = "postgres")]
    fn any_core(&self) -> Result<AtomicCore, AtomicCoreError> {
        let cores = self
            .cores
            .read()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        cores
            .values()
            .next()
            .cloned()
            .ok_or_else(|| AtomicCoreError::Configuration("No cores loaded".to_string()))
    }

    /// Helper: get a storage backend to call database management methods.
    /// In Postgres mode, grabs the storage from any loaded core (they all share a pool).
    #[cfg(feature = "postgres")]
    fn any_storage(&self) -> Result<crate::storage::StorageBackend, AtomicCoreError> {
        self.any_core().map(|c| c.storage.clone())
    }

    /// Resolve a database identifier to its canonical ID.
    /// If the value matches an existing database ID, returns it as-is.
    /// Otherwise, tries a case-insensitive name lookup.
    async fn resolve_database_id(&self, id_or_name: &str) -> Result<String, AtomicCoreError> {
        #[cfg(feature = "postgres")]
        if self.is_postgres() {
            let storage = self.any_storage()?;
            let databases = storage.list_databases_sync().await?;
            if databases.iter().any(|d| d.id == id_or_name) {
                return Ok(id_or_name.to_string());
            }
            if let Some(db) = databases
                .iter()
                .find(|d| d.name.eq_ignore_ascii_case(id_or_name))
            {
                return Ok(db.id.clone());
            }
            return Err(AtomicCoreError::NotFound(format!(
                "Database '{}'",
                id_or_name
            )));
        }

        // SQLite path: check registry
        let registry = self.sqlite_registry();
        let databases = registry.list_databases()?;
        if databases.iter().any(|d| d.id == id_or_name) {
            return Ok(id_or_name.to_string());
        }
        if let Some(db) = registry.find_database_by_name(id_or_name)? {
            return Ok(db.id);
        }
        // Return the original value — let downstream handle not-found
        Ok(id_or_name.to_string())
    }

    /// Get a core for a specific database, loading it lazily if needed.
    /// Accepts either a database ID or name — if `id` doesn't match a known
    /// database ID, it falls back to a case-insensitive name lookup.
    pub async fn get_core(&self, id: &str) -> Result<AtomicCore, AtomicCoreError> {
        // Fast path: already loaded by id
        {
            let cores = self
                .cores
                .read()
                .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            if let Some(core) = cores.get(id) {
                return Ok(core.clone());
            }
        }

        // If the id doesn't look like a known database id, try resolving by name
        let resolved_id = self.resolve_database_id(id).await?;
        if resolved_id != id {
            // Check cache again with the resolved id
            let cores = self
                .cores
                .read()
                .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            if let Some(core) = cores.get(&resolved_id) {
                return Ok(core.clone());
            }
        }
        let id = &resolved_id;

        // Postgres path: create lightweight core sharing the same pool with a new db_id
        #[cfg(feature = "postgres")]
        if let Some(ref _url) = self.database_url {
            // Get the pool from an existing core to share it
            let existing_core = {
                let cores = self
                    .cores
                    .read()
                    .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                cores.values().next().cloned()
            };
            if let Some(existing) = existing_core {
                if let Some(pg) = existing.storage.as_postgres() {
                    let new_pg = pg.with_db_id(id);
                    // Sibling construction shares the existing core's pool
                    // and live provider-config slot, so an explicit config
                    // (and any later `update_provider_config`) covers every
                    // logical database resolved through this manager.
                    let core = existing.sibling_with_storage(new_pg);
                    // Seed default tags for this db_id if needed.
                    let storage = core.storage.clone();
                    let all_tags = storage.get_all_tags_impl().await?;
                    if all_tags.is_empty() {
                        for category in DEFAULT_TAG_CATEGORIES {
                            storage.create_tag_impl(category, None).await?;
                        }
                    }
                    let mut cores = self
                        .cores
                        .write()
                        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                    cores.insert(id.to_string(), core.clone());
                    // No registry to touch in Postgres mode; last_opened_at on
                    // the Postgres `databases` row could be wired up later if needed.
                    return Ok(core);
                }
            }
        }

        // SQLite path: load from disk
        let registry = self.sqlite_registry();
        let db_path = registry.database_path(id);
        let core = AtomicCore::open_for_server_with_registry(&db_path, Some(Arc::clone(registry)))?;

        registry.touch_database(id)?;

        let mut cores = self
            .cores
            .write()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        cores.insert(id.to_string(), core.clone());
        Ok(core)
    }

    /// Get the active (current) database core.
    pub async fn active_core(&self) -> Result<AtomicCore, AtomicCoreError> {
        let id = {
            let guard = self
                .active_id
                .read()
                .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            guard.clone()
        };
        self.get_core(&id).await
    }

    /// Get the active database ID.
    pub fn active_id(&self) -> Result<String, AtomicCoreError> {
        let id = self
            .active_id
            .read()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        Ok(id.clone())
    }

    /// Switch the active database.
    pub async fn set_active(&self, id: &str) -> Result<(), AtomicCoreError> {
        // Validate the database exists
        #[cfg(feature = "postgres")]
        if self.is_postgres() {
            let storage = self.any_storage()?;
            let databases = storage.list_databases_sync().await?;
            if !databases.iter().any(|d| d.id == id) {
                return Err(AtomicCoreError::NotFound(format!("Database '{}'", id)));
            }
        } else {
            let databases = self.sqlite_registry().list_databases()?;
            if !databases.iter().any(|d| d.id == id) {
                return Err(AtomicCoreError::NotFound(format!("Database '{}'", id)));
            }
        }

        #[cfg(not(feature = "postgres"))]
        {
            let databases = self.sqlite_registry().list_databases()?;
            if !databases.iter().any(|d| d.id == id) {
                return Err(AtomicCoreError::NotFound(format!("Database '{}'", id)));
            }
        }

        // Ensure it's loaded
        self.get_core(id).await?;

        let mut active = self
            .active_id
            .write()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        *active = id.to_string();
        Ok(())
    }

    /// Get a reference to the SQLite registry, if one is attached.
    ///
    /// Returns `None` when the manager is running against Postgres — in that mode,
    /// settings, tokens, OAuth, and database metadata all live in Postgres and are
    /// reached via `active_core()` / `get_core()`. Callers that need cross-database
    /// state should prefer the methods on `AtomicCore`, which dispatch to the
    /// right backend automatically.
    pub fn registry(&self) -> Option<&Arc<Registry>> {
        self.registry.as_ref()
    }

    /// Create a new database and register it.
    pub async fn create_database(&self, name: &str) -> Result<DatabaseInfo, AtomicCoreError> {
        #[cfg(feature = "postgres")]
        if self.is_postgres() {
            let existing = self.any_core()?;
            let storage = existing.storage.clone();
            let info = storage.create_database_sync(name).await?;

            // Create a core for the new database (shares Postgres pool, new
            // db_id, and the manager-wide provider-config slot)
            if let Some(pg) = storage.as_postgres() {
                let new_pg = pg.with_db_id(&info.id);
                let core = existing.sibling_with_storage(new_pg);
                // Seed default tags
                let all_tags = core.storage.get_all_tags_impl().await?;
                if all_tags.is_empty() {
                    for category in DEFAULT_TAG_CATEGORIES {
                        core.storage.create_tag_impl(category, None).await?;
                    }
                }
                let mut cores = self
                    .cores
                    .write()
                    .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                cores.insert(info.id.clone(), core);
            }

            return Ok(info);
        }

        let registry = self.sqlite_registry();
        let info = registry.create_database(name)?;

        // Create the actual SQLite file
        let db_path = registry.database_path(&info.id);
        let core = AtomicCore::open_for_server_with_registry(&db_path, Some(Arc::clone(registry)))?;

        let mut cores = self
            .cores
            .write()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        cores.insert(info.id.clone(), core);

        Ok(info)
    }

    /// Delete a database (cannot delete default). Removes from cache and disk.
    pub async fn delete_database(&self, id: &str) -> Result<(), AtomicCoreError> {
        #[cfg(feature = "postgres")]
        if self.is_postgres() {
            // Postgres storage validates it's not the default
            let storage = self.any_storage()?;
            storage.delete_database_sync(id).await?;

            // Remove from cache
            {
                let mut cores = self
                    .cores
                    .write()
                    .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                cores.remove(id);
            }

            // If this was the active database, switch to default
            {
                let active = self
                    .active_id
                    .read()
                    .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                if *active == id {
                    drop(active);
                    let default_id = storage.get_default_database_id_sync().await?;
                    let mut active = self
                        .active_id
                        .write()
                        .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                    *active = default_id;
                }
            }

            // Purge all per-database data rows for this db_id
            storage.purge_database_data_sync(id).await?;
            return Ok(());
        }

        // SQLite path: Registry validates it's not the default
        let registry = self.sqlite_registry();
        registry.delete_database(id)?;

        // Remove from cache
        {
            let mut cores = self
                .cores
                .write()
                .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            if let Some(core) = cores.remove(id) {
                core.optimize();
            }
        }

        // If this was the active database, switch to default
        {
            let active = self
                .active_id
                .read()
                .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            if *active == id {
                drop(active);
                let default_id = registry.get_default_database_id()?;
                let mut active = self
                    .active_id
                    .write()
                    .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                *active = default_id;
            }
        }

        // Delete the file
        let db_path = registry.database_path(id);
        if db_path.exists() {
            std::fs::remove_file(&db_path).ok();
            // Also remove WAL/SHM
            std::fs::remove_file(db_path.with_extension("db-wal")).ok();
            std::fs::remove_file(db_path.with_extension("db-shm")).ok();
        }

        Ok(())
    }

    /// List all databases with their info, plus which is active.
    pub async fn list_databases(&self) -> Result<(Vec<DatabaseInfo>, String), AtomicCoreError> {
        #[cfg(feature = "postgres")]
        if self.is_postgres() {
            let databases = self.any_storage()?.list_databases_sync().await?;
            let active = self
                .active_id
                .read()
                .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
            return Ok((databases, active.clone()));
        }

        let databases = self.sqlite_registry().list_databases()?;
        let active = self
            .active_id
            .read()
            .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
        Ok((databases, active.clone()))
    }

    /// Rename a database.
    pub async fn rename_database(&self, id: &str, name: &str) -> Result<(), AtomicCoreError> {
        #[cfg(feature = "postgres")]
        if self.is_postgres() {
            return self.any_storage()?.rename_database_sync(id, name).await;
        }

        self.sqlite_registry().rename_database(id, name)
    }

    /// Set a database as the new default.
    pub async fn set_default_database(&self, id: &str) -> Result<(), AtomicCoreError> {
        #[cfg(feature = "postgres")]
        if self.is_postgres() {
            return self.any_storage()?.set_default_database_sync(id).await;
        }

        self.sqlite_registry().set_default_database(id)
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

    #[tokio::test]
    async fn test_new_manager() {
        let dir = TempDir::new().unwrap();
        let manager = DatabaseManager::new(dir.path()).unwrap();

        let (databases, active_id) = manager.list_databases().await.unwrap();
        assert_eq!(databases.len(), 1);
        assert_eq!(active_id, "default");
    }

    #[tokio::test]
    async fn test_get_active_core() {
        let dir = TempDir::new().unwrap();
        let manager = DatabaseManager::new(dir.path()).unwrap();

        let core = manager.active_core().await.unwrap();
        // Should be able to query the core
        let settings = core.get_settings().await.unwrap();
        assert!(settings.contains_key("provider"));
    }

    #[tokio::test]
    async fn test_create_and_switch_database() {
        let dir = TempDir::new().unwrap();
        let manager = DatabaseManager::new(dir.path()).unwrap();

        let info = manager.create_database("Work").await.unwrap();
        assert_eq!(info.name, "Work");

        manager.set_active(&info.id).await.unwrap();
        let active = manager.active_id().unwrap();
        assert_eq!(active, info.id);
    }

    #[tokio::test]
    async fn test_delete_database() {
        let dir = TempDir::new().unwrap();
        let manager = DatabaseManager::new(dir.path()).unwrap();

        let info = manager.create_database("Temp").await.unwrap();
        manager.delete_database(&info.id).await.unwrap();

        let (databases, _) = manager.list_databases().await.unwrap();
        assert_eq!(databases.len(), 1); // only default
    }

    #[tokio::test]
    async fn test_delete_active_switches_to_default() {
        let dir = TempDir::new().unwrap();
        let manager = DatabaseManager::new(dir.path()).unwrap();

        let info = manager.create_database("Temp").await.unwrap();
        manager.set_active(&info.id).await.unwrap();
        manager.delete_database(&info.id).await.unwrap();

        let active = manager.active_id().unwrap();
        assert_eq!(active, "default");
    }

    #[tokio::test]
    async fn test_cannot_delete_default() {
        let dir = TempDir::new().unwrap();
        let manager = DatabaseManager::new(dir.path()).unwrap();

        let result = manager.delete_database("default").await;
        assert!(result.is_err());
    }
}
