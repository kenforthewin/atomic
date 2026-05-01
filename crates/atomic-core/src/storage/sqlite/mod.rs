//! SQLite implementation of the Storage traits.
//!
//! This wraps the existing `Database` struct and implements all storage traits
//! by delegating to the existing query functions. Sync rusqlite calls are
//! wrapped in `tokio::task::spawn_blocking` to satisfy the async trait interface.

mod atoms;
mod briefings;
mod chat;
mod chunks;
mod clusters;
mod feeds;
mod search;
mod settings;
mod tags;
mod wiki;
pub(crate) mod health;

use crate::db::Database;
use crate::storage::traits::*;
use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;

/// SQLite-backed storage implementation.
///
/// Wraps the existing `Database` struct and implements all storage traits.
/// All async methods use `tokio::task::spawn_blocking` internally to wrap
/// the synchronous rusqlite operations.
#[derive(Clone)]
pub struct SqliteStorage {
    pub(crate) db: Arc<Database>,
}

impl SqliteStorage {
    /// Create a new SqliteStorage wrapping an existing Database.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Get a reference to the underlying Database (for internal use during migration).
    pub fn database(&self) -> &Arc<Database> {
        &self.db
    }
}

#[async_trait]
impl Storage for SqliteStorage {
    async fn initialize(&self) -> StorageResult<()> {
        // Database is already initialized by Database::open / open_or_create
        Ok(())
    }

    async fn shutdown(&self) -> StorageResult<()> {
        self.db.optimize();
        Ok(())
    }

    fn storage_path(&self) -> &Path {
        &self.db.db_path
    }
}
