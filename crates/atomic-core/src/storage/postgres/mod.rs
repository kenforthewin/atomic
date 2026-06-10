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
mod retry;
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
#[cfg(feature = "postgres")]
use std::time::Duration;

/// Tunables for the Postgres connection pool.
///
/// Defaults match the historical hard-coded values (50 connections, 10s
/// acquire timeout, no idle/lifetime caps) so existing deployments keep their
/// behavior. Override via the `ATOMIC_PG_*` environment variables or by
/// constructing this struct directly and calling
/// [`PostgresStorage::connect_with_config`].
///
/// Environment variables (all optional):
///   - `ATOMIC_PG_MAX_CONNECTIONS` — pool max size
///   - `ATOMIC_PG_ACQUIRE_TIMEOUT_SECS` — wait time before acquire fails
///   - `ATOMIC_PG_IDLE_TIMEOUT_SECS` — close idle connections after N seconds
///   - `ATOMIC_PG_MAX_LIFETIME_SECS` — recycle connections after N seconds
///   - `ATOMIC_PG_SLOW_QUERY_MS` — log queries slower than this at WARN
///     (set to 0 to disable; default 1000ms)
#[cfg(feature = "postgres")]
#[derive(Debug, Clone)]
pub struct PgPoolConfig {
    pub max_connections: u32,
    pub acquire_timeout: Duration,
    pub idle_timeout: Option<Duration>,
    pub max_lifetime: Option<Duration>,
    /// When set, sqlx logs any statement slower than this threshold at WARN
    /// via `tracing` (target = `sqlx::query`). Setting this to `None` (env
    /// value `0`) disables slow-query logging entirely.
    pub slow_query_threshold: Option<Duration>,
}

#[cfg(feature = "postgres")]
impl PgPoolConfig {
    /// Build a config from `ATOMIC_PG_*` environment variables, falling back
    /// to library defaults for anything unset.
    pub fn from_env() -> Self {
        fn parse_secs(name: &str) -> Option<Duration> {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs)
        }
        let slow_query_threshold = std::env::var("ATOMIC_PG_SLOW_QUERY_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(|ms| if ms == 0 { None } else { Some(Duration::from_millis(ms)) })
            .unwrap_or(Some(Duration::from_millis(1000)));
        Self {
            max_connections: std::env::var("ATOMIC_PG_MAX_CONNECTIONS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(50),
            acquire_timeout: parse_secs("ATOMIC_PG_ACQUIRE_TIMEOUT_SECS")
                .unwrap_or_else(|| Duration::from_secs(10)),
            idle_timeout: parse_secs("ATOMIC_PG_IDLE_TIMEOUT_SECS"),
            max_lifetime: parse_secs("ATOMIC_PG_MAX_LIFETIME_SECS"),
            slow_query_threshold,
        }
    }
}

#[cfg(feature = "postgres")]
impl Default for PgPoolConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

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
    /// Connect to a Postgres database with a specific logical database ID,
    /// using pool tunables drawn from `ATOMIC_PG_*` environment variables.
    pub async fn connect(database_url: &str, db_id: &str) -> Result<Self, AtomicCoreError> {
        Self::connect_with_config(database_url, db_id, PgPoolConfig::from_env()).await
    }

    /// Connect with an explicit pool configuration. Bypasses the
    /// environment-variable fallback chain in `connect`.
    pub async fn connect_with_config(
        database_url: &str,
        db_id: &str,
        config: PgPoolConfig,
    ) -> Result<Self, AtomicCoreError> {
        use sqlx::postgres::PgPoolOptions;
        use sqlx::ConnectOptions;
        use std::str::FromStr;

        let mut connect_opts = sqlx::postgres::PgConnectOptions::from_str(database_url)
            .map_err(|e| {
                AtomicCoreError::DatabaseOperation(format!(
                    "Invalid Postgres connection URL: {}",
                    e
                ))
            })?;

        // Quiet by default. Atomic emits its own structured logs at the call
        // sites that matter; sqlx's `Executed query` line is noisy and only
        // useful in debug. Slow-query logging stays on (configurable below).
        connect_opts = connect_opts.log_statements(log::LevelFilter::Off);
        if let Some(threshold) = config.slow_query_threshold {
            connect_opts =
                connect_opts.log_slow_statements(log::LevelFilter::Warn, threshold);
        } else {
            connect_opts =
                connect_opts.log_slow_statements(log::LevelFilter::Off, Duration::default());
        }

        let mut opts = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .acquire_timeout(config.acquire_timeout);
        if let Some(idle) = config.idle_timeout {
            opts = opts.idle_timeout(idle);
        }
        if let Some(lifetime) = config.max_lifetime {
            opts = opts.max_lifetime(lifetime);
        }

        let pool = opts.connect_with(connect_opts).await.map_err(|e| {
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
            (
                19,
                include_str!("migrations/019_atom_chunks_hnsw_index.sql"),
            ),
            (
                20,
                include_str!("migrations/020_atom_positions_double.sql"),
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
