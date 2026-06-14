//! Control-plane database handle and migration runner.
//!
//! The control plane is a dedicated Postgres database on the shared cluster
//! (default name [`DEFAULT_CONTROL_DB_NAME`]) holding cross-tenant state:
//! accounts, account→tenant-database mappings, tokens, sessions, subdomain
//! reservations, and encrypted provider credentials. It is deliberately
//! separate from every tenant
//! database — tenant databases run atomic-core's migrations; the control
//! plane runs its own, embedded here.
//!
//! # Migration discipline
//!
//! Control-plane migrations are **additive-only** (plan: "Schema migration
//! on deploy"): ADD COLUMN, CREATE TABLE, CREATE INDEX, deferred/
//! not-validated constraints. No DROP COLUMN, no ALTER COLUMN TYPE, no
//! renames. Rolling deploys depend on this — an old binary must be able to
//! read a schema one version ahead of it. Drops happen N+1 deploys later,
//! after all referring code has left the fleet.
//!
//! The runner mirrors the hardened pattern in
//! `atomic-core/src/storage/postgres/mod.rs`: a `schema_version` table
//! records applied versions (each migration's SQL inserts its own row), a
//! session-level advisory lock serializes concurrent callers, and every
//! error propagates — a failed version read is never treated as version 0.

use std::str::FromStr;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, PgConnection, PgPool};

use crate::error::CloudError;

/// Default name of the control-plane database when the configured URL
/// doesn't specify one.
pub const DEFAULT_CONTROL_DB_NAME: &str = "atomic_cloud_control";

/// Embedded migration registry: `(version, sql)`. Each migration's SQL is
/// responsible for inserting its own `schema_version` row, matching the
/// tenant-migration convention in atomic-core.
const MIGRATIONS: &[(i32, &str)] = &[
    (1, include_str!("../migrations/001_control_plane.sql")),
    (2, include_str!("../migrations/002_magic_links.sql")),
    (
        3,
        include_str!("../migrations/003_subdomain_reservation_age.sql"),
    ),
    (
        4,
        include_str!("../migrations/004_provider_credentials.sql"),
    ),
    (5, include_str!("../migrations/005_provider_generation.sql")),
    (6, include_str!("../migrations/006_dispatch_hints.sql")),
    (
        7,
        include_str!("../migrations/007_provider_backpressure.sql"),
    ),
    (8, include_str!("../migrations/008_migration_tracking.sql")),
    (9, include_str!("../migrations/009_deploy_runs.sql")),
    (10, include_str!("../migrations/010_plans_billing.sql")),
    (
        11,
        include_str!("../migrations/011_webhook_idempotency.sql"),
    ),
    (12, include_str!("../migrations/012_trials.sql")),
    (
        13,
        include_str!("../migrations/013_storage_enforcement.sql"),
    ),
    (14, include_str!("../migrations/014_oauth.sql")),
    (15, include_str!("../migrations/015_backups.sql")),
    (16, include_str!("../migrations/016_backup_run_status.sql")),
];

/// Advisory lock key serializing control-plane migrations. Advisory locks
/// are scoped to the database a session is connected to, so this cannot
/// collide with atomic-core's tenant-migration lock — a distinct constant
/// just keeps `pg_locks` output unambiguous. ASCII "atm_ctrl".
const MIGRATION_LOCK_KEY: i64 = 0x61746d5f6374726c;

/// Handle to the control-plane database.
///
/// Cheap to clone — wraps an [`sqlx::PgPool`]. The pool is intentionally
/// small: the control plane serves point lookups (subdomain → account,
/// token verification) and pgbouncer fronts the cluster in production.
#[derive(Clone)]
pub struct ControlPlane {
    pool: PgPool,
}

impl ControlPlane {
    /// Connect to the control-plane database, creating it on the cluster if
    /// it doesn't exist yet (first boot, fresh test clusters).
    ///
    /// `control_db_url` is a full Postgres URL. When its path component
    /// omits a database name, [`DEFAULT_CONTROL_DB_NAME`] is used. Creation
    /// is race-safe: a concurrent `CREATE DATABASE` losing the race is
    /// treated as success.
    ///
    /// This only establishes the pool — call [`initialize`](Self::initialize)
    /// to run pending migrations.
    pub async fn connect(control_db_url: &str) -> Result<Self, CloudError> {
        let opts = control_options(control_db_url)?;
        ensure_database_exists(&opts).await?;

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(opts)
            .await
            .map_err(CloudError::db("connecting to control-plane database"))?;
        Ok(Self { pool })
    }

    /// The underlying connection pool, for control-plane queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Wrap an existing pool — for unit tests of components whose pure
    /// logic never touches the database (e.g. the breaker's detection
    /// window). Never used by production code paths.
    #[cfg(test)]
    pub(crate) fn from_pool_for_tests(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Resolve a subdomain to its `accounts.id`, regardless of account
    /// status. Operator tooling (the CLI addresses accounts by subdomain);
    /// the request path uses its own status-aware lookup in the auth
    /// middleware.
    pub async fn account_id_by_subdomain(
        &self,
        subdomain: &str,
    ) -> Result<Option<String>, CloudError> {
        sqlx::query_scalar("SELECT id FROM accounts WHERE subdomain = $1")
            .bind(subdomain)
            .fetch_optional(&self.pool)
            .await
            .map_err(CloudError::db("looking up account by subdomain"))
    }

    /// Run pending control-plane migrations; returns how many were applied
    /// (zero when the schema is already current).
    ///
    /// Safe to call from any number of processes concurrently: callers
    /// serialize on a session-level advisory lock, and each one re-reads
    /// `schema_version` after acquiring it, so losers of the race apply
    /// nothing.
    pub async fn initialize(&self) -> Result<u32, CloudError> {
        // The advisory lock is session-level, so it must be released by the
        // same session that took it. Pin it to a connection detached from
        // the pool: if this future is cancelled, the owned connection drops,
        // its session closes, and the lock releases — instead of a
        // lock-holding connection returning to the pool and deadlocking
        // every later caller. (Same reasoning as atomic-core's tenant
        // migration runner; see storage/postgres/mod.rs.)
        let mut lock_conn = self
            .pool
            .acquire()
            .await
            .map_err(CloudError::db("acquiring migration lock connection"))?
            .detach();

        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(MIGRATION_LOCK_KEY)
            .execute(&mut lock_conn)
            .await
            .map_err(CloudError::db("acquiring migration advisory lock"))?;

        let result = self.run_migrations_inner().await;

        // Closing the lock connection ends its session, which releases the
        // advisory lock even if an explicit unlock would have failed.
        let _ = lock_conn.close().await;

        result
    }

    async fn run_migrations_inner(&self) -> Result<u32, CloudError> {
        // Errors reading the current version must propagate, never default
        // to 0: treating a failed read as "fresh database" would re-run
        // every migration against an already-populated schema.
        let table_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM information_schema.tables \
             WHERE table_name = 'schema_version')",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(CloudError::db("checking for schema_version table"))?;

        // `schema_version.version` is INTEGER (int4); the decode type must
        // match exactly or sqlx's strict type check rejects the row.
        let current_version: i32 = if table_exists {
            sqlx::query_scalar::<_, i32>("SELECT COALESCE(MAX(version), 0) FROM schema_version")
                .fetch_one(&self.pool)
                .await
                .map_err(CloudError::db("reading schema_version"))?
        } else {
            0
        };

        let mut applied = 0u32;
        for &(version, sql) in MIGRATIONS {
            if version > current_version {
                sqlx::raw_sql(sql)
                    .execute(&self.pool)
                    .await
                    .map_err(CloudError::db(format!(
                        "control-plane migration {version} failed"
                    )))?;
                applied += 1;
            }
        }

        Ok(applied)
    }
}

/// Parse the configured URL into connect options, defaulting the database
/// name to [`DEFAULT_CONTROL_DB_NAME`] when the URL omits one.
fn control_options(control_db_url: &str) -> Result<PgConnectOptions, CloudError> {
    let opts = PgConnectOptions::from_str(control_db_url)
        .map_err(|e| CloudError::InvalidUrl(format!("control-plane database URL: {e}")))?;
    Ok(match opts.get_database() {
        Some(_) => opts,
        None => opts.database(DEFAULT_CONTROL_DB_NAME),
    })
}

/// Create the control-plane database if it doesn't exist, via a short-lived
/// connection to the cluster's `postgres` maintenance database.
///
/// `CREATE DATABASE` cannot run inside a transaction and cannot bind its
/// identifier as a parameter, hence the plain connection, the name
/// validation, and the quoted interpolation. A concurrent creator winning
/// the race surfaces as SQLSTATE 42P04 (`duplicate_database`) and is
/// treated as success.
async fn ensure_database_exists(opts: &PgConnectOptions) -> Result<(), CloudError> {
    let db_name = opts
        .get_database()
        .expect("control_options always sets a database name")
        .to_string();
    if !is_safe_database_name(&db_name) {
        return Err(CloudError::InvalidDatabaseName(db_name));
    }

    let mut conn = PgConnection::connect_with(&opts.clone().database("postgres"))
        .await
        .map_err(CloudError::db(
            "connecting to maintenance database to check control plane exists",
        ))?;

    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(&db_name)
            .fetch_one(&mut conn)
            .await
            .map_err(CloudError::db("checking control-plane database existence"))?;

    if !exists {
        let create = sqlx::raw_sql(&format!("CREATE DATABASE \"{db_name}\""))
            .execute(&mut conn)
            .await;
        match create {
            Ok(_) => {
                tracing::info!(db_name, "created control-plane database");
            }
            // 42P04 duplicate_database: a concurrent boot won the race.
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("42P04") => {}
            Err(e) => {
                return Err(CloudError::db("creating control-plane database")(e));
            }
        }
    }

    let _ = conn.close().await;
    Ok(())
}

/// Database names are interpolated into DDL as quoted identifiers; restrict
/// them to a charset that can't escape the quoting.
fn is_safe_database_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_options_defaults_database_name() {
        let opts = control_options("postgres://user:pw@localhost:5433").unwrap();
        assert_eq!(opts.get_database(), Some(DEFAULT_CONTROL_DB_NAME));
    }

    #[test]
    fn control_options_respects_explicit_database_name() {
        let opts = control_options("postgres://user:pw@localhost:5433/custom_ctrl").unwrap();
        assert_eq!(opts.get_database(), Some("custom_ctrl"));
    }

    #[test]
    fn rejects_unsafe_database_names() {
        assert!(is_safe_database_name("atomic_cloud_control"));
        assert!(is_safe_database_name("acct-1234"));
        assert!(!is_safe_database_name(""));
        assert!(!is_safe_database_name("evil\"; DROP DATABASE x; --"));
        assert!(!is_safe_database_name("name with spaces"));
    }
}
