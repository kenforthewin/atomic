//! Per-tenant schema-migration tracking (plan: "Provisioning lifecycle" →
//! "Schema migration on deploy", steps 1-3 + "Stragglers").
//!
//! One tenant = one Postgres database, all running atomic-core's tenant
//! migrations — so a binary upgrade is a *fleet* migration: every tenant
//! database must be brought to the new binary's compiled schema target.
//! Migration 008 adds the tracking columns to `account_databases`; this
//! module is the typed query surface over them, shared by three consumers:
//!
//! - **The boot-time fleet runner** enumerates lagging tenants
//!   ([`list_unmigrated`]), runs `storage.initialize()` per tenant (safe to
//!   race across pods — atomic-core's migration runner serializes on a
//!   per-database advisory lock), and records each outcome
//!   ([`record_migration_success`] / [`record_migration_failure`]).
//! - **The reaper's failed-migrations arm** retries rows whose
//!   `migration_retry_after` has passed, through the same record functions.
//! - **CloudAuth's straggler gate** reads `last_migrated_version` on its
//!   per-request account lookup and returns the structured 503
//!   `account_upgrading` while a tenant lags [`tenant_schema_target`] (see
//!   `crate::auth`).
//!
//! [`provision_account`](crate::provision::provision_account) stamps the
//! compiled target when it writes the `account_databases` row (the tenant
//! was fully migrated two steps earlier), so fresh tenants are never
//! stragglers.

use atomic_core::storage::PostgresStorage;
use chrono::{DateTime, Utc};

use crate::control_plane::ControlPlane;
use crate::error::CloudError;

/// The tenant schema version this binary brings tenant databases to —
/// atomic-core's compiled migration target. Everything in the crate that
/// compares or stamps `last_migrated_version` goes through this one
/// chokepoint so the gate, the stamp, and the runner can never disagree.
pub fn tenant_schema_target() -> i32 {
    PostgresStorage::target_schema_version()
}

/// Stored `last_migration_error` texts are bounded to this many characters
/// (same hygiene as BYOK validation errors): migration failures embed
/// driver/SQL error chains of unbounded size, and the column exists for
/// operator triage, not log archival.
pub const MIGRATION_ERROR_MAX_LEN: usize = 500;

/// An `account_databases` row lagging the compiled schema target — one unit
/// of work for the fleet runner or the reaper's failed-migrations arm.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UnmigratedTenant {
    pub account_id: String,
    pub db_name: String,
    /// The version the tenant was last successfully migrated to.
    pub last_migrated_version: i32,
    /// When the most recent attempt failed; `None` when the row simply
    /// hasn't been attempted since the binary's target moved.
    pub migration_failed_at: Option<DateTime<Utc>>,
    /// Reaper backoff horizon for failed rows.
    pub migration_retry_after: Option<DateTime<Utc>>,
    /// Consecutive failures since the last success.
    pub migration_retry_count: i32,
}

/// Plan step 1: enumerate active tenants whose schema lags `target`,
/// oldest-version first (the furthest-behind tenants have the most pending
/// work; start them earliest). Non-`active` mapping rows are excluded —
/// there is nothing to serve, so nothing to gate or migrate.
pub async fn list_unmigrated(
    control: &ControlPlane,
    target: i32,
) -> Result<Vec<UnmigratedTenant>, CloudError> {
    sqlx::query_as(
        "SELECT account_id, db_name, last_migrated_version, migration_failed_at, \
                migration_retry_after, migration_retry_count \
         FROM account_databases \
         WHERE status = 'active' AND last_migrated_version < $1 \
         ORDER BY last_migrated_version ASC, account_id ASC",
    )
    .bind(target)
    .fetch_all(control.pool())
    .await
    .map_err(CloudError::db("listing unmigrated tenants"))
}

/// Plan step 3, success arm: record that `db_name` was brought to `version`
/// and clear all failure/backoff state.
///
/// `GREATEST` keeps the recorded version monotone under rolling deploys: an
/// old binary (target N) racing a new one (target N+1) over the same tenant
/// must not regress the stamp and re-flag an already-upgraded tenant as a
/// straggler to the new pods.
pub async fn record_migration_success(
    control: &ControlPlane,
    account_id: &str,
    db_name: &str,
    version: i32,
) -> Result<(), CloudError> {
    sqlx::query(
        "UPDATE account_databases \
         SET last_migrated_version = GREATEST(last_migrated_version, $3), \
             last_migrated_at = NOW(), \
             migration_failed_at = NULL, \
             last_migration_error = NULL, \
             migration_retry_after = NULL, \
             migration_retry_count = 0 \
         WHERE account_id = $1 AND db_name = $2",
    )
    .bind(account_id)
    .bind(db_name)
    .bind(version)
    .execute(control.pool())
    .await
    .map_err(CloudError::db("recording tenant migration success"))?;
    Ok(())
}

/// Plan step 3, failure arm: record a failed attempt — the (bounded) error
/// text, the failure time, the reaper's next-retry horizon, and a bumped
/// retry count. `last_migrated_version` is untouched: the tenant is exactly
/// as migrated as it was before the attempt.
pub async fn record_migration_failure(
    control: &ControlPlane,
    account_id: &str,
    db_name: &str,
    error: &str,
    retry_after: DateTime<Utc>,
) -> Result<(), CloudError> {
    sqlx::query(
        "UPDATE account_databases \
         SET migration_failed_at = NOW(), \
             last_migration_error = $3, \
             migration_retry_after = $4, \
             migration_retry_count = migration_retry_count + 1 \
         WHERE account_id = $1 AND db_name = $2",
    )
    .bind(account_id)
    .bind(db_name)
    .bind(truncate_error(error))
    .bind(retry_after)
    .execute(control.pool())
    .await
    .map_err(CloudError::db("recording tenant migration failure"))?;
    Ok(())
}

/// Bound an error message to [`MIGRATION_ERROR_MAX_LEN`] characters on a
/// char boundary.
fn truncate_error(error: &str) -> String {
    error.chars().take(MIGRATION_ERROR_MAX_LEN).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Migration 008 backfills pre-existing `account_databases` rows with
    /// the frozen literal 22 — the compiled tenant target at authoring time.
    /// Its safety argument (008's header comment) is that 22 is at-or-below
    /// every tenant's true version, which holds as long as atomic-core's
    /// registry never rewinds below it. Pin that.
    #[test]
    fn frozen_backfill_stamp_is_at_or_below_the_compiled_target() {
        assert!(
            tenant_schema_target() >= 22,
            "atomic-core's tenant migration registry rewound below 22; \
             migration 008's backfill stamp is no longer at-or-below the \
             compiled target and its safety argument breaks"
        );
    }

    #[test]
    fn error_truncation_is_bounded_and_char_safe() {
        let short = "tenant database unreachable";
        assert_eq!(truncate_error(short), short);

        // Multi-byte chars near the boundary must not split.
        let long = "é".repeat(MIGRATION_ERROR_MAX_LEN + 100);
        let truncated = truncate_error(&long);
        assert_eq!(truncated.chars().count(), MIGRATION_ERROR_MAX_LEN);
    }
}
