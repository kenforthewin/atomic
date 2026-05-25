//! Postgres + pgvector implementation of the Storage traits.
//!
//! This module provides a PostgresStorage backend using sqlx with pgvector
//! for vector similarity search and Postgres built-in tsvector for full-text search.
//! All methods are natively async (no spawn_blocking needed).

#[cfg(feature = "postgres")]
mod atoms;
#[cfg(feature = "postgres")]
#[cfg(feature = "postgres")]
mod chat;
#[cfg(feature = "postgres")]
mod chunks;
#[cfg(feature = "postgres")]
mod clusters;
#[cfg(feature = "postgres")]
mod feeds;
#[cfg(feature = "postgres")]
mod oauth;
#[cfg(feature = "postgres")]
mod reports;
#[cfg(feature = "postgres")]
mod search;
#[cfg(feature = "postgres")]
mod settings;
#[cfg(feature = "postgres")]
mod tags;
#[cfg(feature = "postgres")]
mod task_runs;
#[cfg(feature = "postgres")]
mod wiki;

#[cfg(feature = "postgres")]
use crate::error::AtomicCoreError;
#[cfg(feature = "postgres")]
use crate::storage::traits::*;
#[cfg(feature = "postgres")]
use async_trait::async_trait;
#[cfg(feature = "postgres")]
use sqlx::PgPool;

/// Postgres-backed storage implementation using sqlx + pgvector.
///
/// Each instance is scoped to a `db_id` for multi-database isolation.
/// Multiple `PostgresStorage` instances can share the same `PgPool`
/// with different `db_id` values for logical separation.
#[cfg(feature = "postgres")]
#[derive(Clone)]
pub struct PostgresStorage {
    pub(crate) pool: PgPool,
    /// Logical database ID — all queries are scoped to this value.
    pub(crate) db_id: String,
}

#[cfg(feature = "postgres")]
impl PostgresStorage {
    /// Connect to a Postgres database with a specific logical database ID.
    ///
    /// Creates a connection pool on the caller's tokio runtime.
    pub async fn connect(database_url: &str, db_id: &str) -> Result<Self, AtomicCoreError> {
        use sqlx::postgres::PgPoolOptions;

        let pool = PgPoolOptions::new()
            .max_connections(50)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(database_url)
            .await
            .map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!("Postgres connection failed: {}", e))
            })?;
        Ok(Self {
            pool,
            db_id: db_id.to_string(),
        })
    }

    /// Create a new PostgresStorage sharing the same pool but with a different db_id.
    /// Used by DatabaseManager to create lightweight cores for different databases.
    pub fn with_db_id(&self, db_id: &str) -> Self {
        Self {
            pool: self.pool.clone(),
            db_id: db_id.to_string(),
        }
    }

    /// Get a reference to the connection pool (for test cleanup, etc.)
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Initialize the schema — runs migrations on the caller's runtime.
    pub async fn initialize(&self) -> Result<(), AtomicCoreError> {
        self.run_migrations().await
    }

    /// Run migrations incrementally based on schema_version.
    /// Uses a Postgres advisory lock to serialize concurrent callers
    /// (e.g., parallel test threads).
    async fn run_migrations(&self) -> Result<(), AtomicCoreError> {
        // Migration registry: (version, sql)
        let migrations: &[(i32, &str)] = &[
            (1, include_str!("migrations/001_initial.sql")),
            (2, include_str!("migrations/002_add_db_id.sql")),
            (3, include_str!("migrations/003_add_error_columns.sql")),
            (4, include_str!("migrations/004_wiki_proposals.sql")),
            (5, include_str!("migrations/005_autotag_target.sql")),
            (6, include_str!("migrations/006_oauth.sql")),
            (7, include_str!("migrations/007_briefings.sql")),
            (8, include_str!("migrations/008_global_search_vectors.sql")),
            (9, include_str!("migrations/009_atom_links.sql")),
            (10, include_str!("migrations/010_pipeline_jobs.sql")),
            (11, include_str!("migrations/011_edges_status.sql")),
            (12, include_str!("migrations/012_autotag_description.sql")),
            (13, include_str!("migrations/013_atom_tags_source.sql")),
            (14, include_str!("migrations/014_atoms_kind.sql")),
            (15, include_str!("migrations/015_task_runs.sql")),
            (16, include_str!("migrations/016_reports.sql")),
            (
                17,
                include_str!("migrations/017_task_runs_active_unique.sql"),
            ),
            (18, include_str!("migrations/018_briefings_teardown.sql")),
            (19, include_str!("migrations/019_knowledge_signals.sql")),
            (
                20,
                include_str!("migrations/020_knowledge_signal_action_log.sql"),
            ),
        ];

        // Advisory lock key — arbitrary fixed i64 to serialize migrations
        const MIGRATION_LOCK_KEY: i64 = 0x61746f6d69635f6d; // "atomic_m"

        // Acquire advisory lock (session-level, blocks until available)
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(MIGRATION_LOCK_KEY)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!(
                    "Failed to acquire migration lock: {}",
                    e
                ))
            })?;

        let result = self.run_migrations_inner(migrations).await;

        // Release advisory lock regardless of outcome
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(MIGRATION_LOCK_KEY)
            .execute(&self.pool)
            .await
            .ok();

        result
    }

    async fn run_migrations_inner(
        &self,
        migrations: &[(i32, &str)],
    ) -> Result<(), AtomicCoreError> {
        // Check if schema_version table exists
        let table_exists: bool = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM information_schema.tables WHERE table_name = 'schema_version')"
        )
        .fetch_one(&self.pool)
        .await
        .unwrap_or(false);

        let current_version: i32 = if table_exists {
            sqlx::query_scalar::<_, i64>("SELECT COALESCE(MAX(version), 0) FROM schema_version")
                .fetch_one(&self.pool)
                .await
                .unwrap_or(0) as i32
        } else {
            0
        };

        for &(version, sql) in migrations {
            if version > current_version {
                sqlx::raw_sql(sql).execute(&self.pool).await.map_err(|e| {
                    AtomicCoreError::DatabaseOperation(format!(
                        "Migration {} failed: {}",
                        version, e
                    ))
                })?;
            }
        }

        Ok(())
    }
}

#[cfg(feature = "postgres")]
#[async_trait]
impl Storage for PostgresStorage {
    async fn initialize(&self) -> StorageResult<()> {
        self.run_migrations().await
    }

    async fn shutdown(&self) -> StorageResult<()> {
        self.pool.close().await;
        Ok(())
    }

    fn storage_path(&self) -> &std::path::Path {
        // Postgres doesn't have a file path; return a placeholder
        std::path::Path::new("postgres")
    }
}
