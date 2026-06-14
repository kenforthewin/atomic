//! The nightly backup pass, the final dump, and backup bookkeeping (plan:
//! "Backups & disaster recovery" → "v1: nightly logical dumps").
//!
//! This module is the orchestration layer over the two seams that do the
//! actual work — [`crate::backup`] (the `pg_dump`/`pg_restore` runner) and
//! [`crate::backup_store`] (where the bytes land). It owns:
//!
//! - The deterministic object keys (`backups/<date>/acct_<uuid>.dump`,
//!   `backups/<date>/control.dump`, `backups/final/<uuid>-<ts>.dump`).
//! - The typed control-plane queries that record per-tenant backup status
//!   and the `backup_runs` ledger.
//! - [`run_backup_pass`] — the nightly fleet pass, mirroring the reaper's
//!   shape: every tenant is dumped under its own per-account advisory lock so
//!   two pods never dump the same tenant at once, the control plane is dumped
//!   once per pass, and the whole thing is summarized in an observable
//!   [`BackupSummary`] and recorded as one `backup_runs` row.
//! - [`final_dump_before_delete`] — the fail-closed dump taken **before**
//!   `DROP DATABASE` in the active-account deletion path (the operator's only
//!   undo under hard-delete v1).
//! - [`stale_tenant_backups`] — the staleness monitor's query ("alert when
//!   any tenant's last successful backup is >36h old").
//!
//! Retention (14 daily + 8 weekly; 30-day finals) is **bucket lifecycle
//! policy, not code** (plan): nothing here ever deletes a backup.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::Connection;
use uuid::Uuid;

use crate::backup::{dump_control_database, dump_tenant_database, DumpConnection};
use crate::backup_store::BackupStore;
use crate::control_plane::{ControlPlane, DEFAULT_CONTROL_DB_NAME};
use crate::error::CloudError;
use crate::provision::{is_tenant_db_name, ClusterConfig};
use crate::reaper::try_account_advisory_lock;

/// Default staleness alert horizon (plan: ">36h old"). The monitor compares
/// each active tenant's `last_backup_at` against `now - this`.
pub const DEFAULT_STALENESS_HORIZON: Duration = Duration::from_secs(36 * 60 * 60);

/// Max successful tenant dumps per nightly pass before deferring the rest to
/// the next pass. Each dump shells out to `pg_dump` (seconds to minutes on a
/// large tenant); a per-pass cap keeps one pod's pass bounded, and the next
/// pass picks up whatever this one didn't reach (its `last_backup_at` is
/// older, so the stale-first ordering surfaces it). Generous by default —
/// v1 fleets are small — and overridable via the CLI.
pub const DEFAULT_MAX_BACKUPS_PER_PASS: usize = 256;

/// An active tenant database the nightly pass backs up.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BackupTarget {
    pub account_id: String,
    pub db_name: String,
}

/// A tenant whose last successful backup is older than the staleness horizon
/// (or who has never been backed up). The monitor's alert payload.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StaleBackup {
    pub account_id: String,
    pub db_name: String,
    /// `None` means never backed up — stale by construction once the tenant
    /// is older than the horizon.
    pub last_backup_at: Option<DateTime<Utc>>,
}

/// What a nightly pass did, for logging and tests. Advisory-lock skips are
/// observable here (like [`ReaperSummary`](crate::reaper::ReaperSummary)), so
/// the cross-pod concurrency test can prove a tenant is never double-dumped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackupSummary {
    /// Tenant databases dumped and uploaded this pass (account ids).
    pub tenants_backed_up: Vec<String>,
    /// Tenants skipped because another pod holds their advisory lock.
    pub tenants_skipped_locked: Vec<String>,
    /// Tenants past [`max_backups_per_pass`](BackupConfig::max_backups_per_pass),
    /// deferred to the next pass untouched.
    pub tenants_deferred: Vec<String>,
    /// Tenants whose dump or upload failed (account ids); the per-tenant
    /// error is recorded on the row and the failure surfaced in `errors`.
    pub tenants_failed: Vec<String>,
    /// Whether the control-plane dump succeeded this pass.
    pub control_backed_up: bool,
    /// Per-target failures, with bounded context (never the password).
    pub errors: Vec<String>,
}

impl BackupSummary {
    /// True when the pass backed nothing up and hit no error — only possible
    /// with an empty fleet and a control-dump failure suppressed, so in
    /// practice a healthy pass is never quiet (the control plane always
    /// dumps). Used to log empty passes at debug.
    pub fn is_quiet(&self) -> bool {
        self.tenants_backed_up.is_empty()
            && self.tenants_skipped_locked.is_empty()
            && self.tenants_deferred.is_empty()
            && self.tenants_failed.is_empty()
            && !self.control_backed_up
            && self.errors.is_empty()
    }
}

/// Tunables for a nightly pass. [`Default`] is production; tests shrink the
/// cap.
#[derive(Debug, Clone)]
pub struct BackupConfig {
    pub max_backups_per_pass: usize,
    /// Staleness horizon for [`stale_tenant_backups`] — not used by the pass
    /// itself, carried here so `serve` configures one place.
    pub staleness_horizon: Duration,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            max_backups_per_pass: DEFAULT_MAX_BACKUPS_PER_PASS,
            staleness_horizon: DEFAULT_STALENESS_HORIZON,
        }
    }
}

// ==================== Object keys ====================

/// `backups/<date>/acct_<uuid>.dump` — one tenant's nightly dump. `date` is
/// the pass's UTC calendar day, so a day's re-run overwrites (idempotent) and
/// bucket lifecycle can age out by the date prefix.
pub fn nightly_tenant_key(date: DateTime<Utc>, db_name: &str) -> String {
    format!("backups/{}/{}.dump", date.format("%Y-%m-%d"), db_name)
}

/// `backups/<date>/control.dump` — the control plane's nightly dump.
pub fn nightly_control_key(date: DateTime<Utc>) -> String {
    format!("backups/{}/control.dump", date.format("%Y-%m-%d"))
}

/// `backups/final/<uuid>-<ts>.dump` — the final dump before an account
/// deletion. The timestamp keeps repeated deletions of re-created subdomains
/// (or a retried deletion) from colliding, and the flat `final/` prefix lets
/// bucket lifecycle apply the 30-day final retention independently of the
/// dated nightly tree.
pub fn final_key(account_id: &str, ts: DateTime<Utc>) -> String {
    format!(
        "backups/final/{account_id}-{}.dump",
        ts.format("%Y%m%dT%H%M%SZ")
    )
}

// ==================== Control-plane queries ====================

/// Active tenant databases the nightly pass backs up, stale-first
/// (never-backed-up sorts first via `NULLS FIRST`) so a capped pass makes
/// progress on the most-overdue tenants. Only `status = 'active'` rows: a
/// provisioning/half-built tenant may not be dumpable.
pub async fn list_active_tenant_databases(
    control: &ControlPlane,
) -> Result<Vec<BackupTarget>, CloudError> {
    sqlx::query_as(
        "SELECT account_id, db_name FROM account_databases \
         WHERE status = 'active' \
         ORDER BY last_backup_at ASC NULLS FIRST, account_id",
    )
    .fetch_all(control.pool())
    .await
    .map_err(CloudError::db("listing active tenant databases for backup"))
}

/// Record a successful backup of `db_name` for `account_id` at `at`, clearing
/// any prior error.
pub async fn record_backup_success(
    control: &ControlPlane,
    account_id: &str,
    db_name: &str,
    at: DateTime<Utc>,
) -> Result<(), CloudError> {
    sqlx::query(
        "UPDATE account_databases \
         SET last_backup_at = $3, last_backup_error = NULL \
         WHERE account_id = $1 AND db_name = $2",
    )
    .bind(account_id)
    .bind(db_name)
    .bind(at)
    .execute(control.pool())
    .await
    .map_err(CloudError::db("recording backup success"))?;
    Ok(())
}

/// Record a failed backup of `db_name` for `account_id`. `last_backup_at` is
/// left untouched — the staleness monitor must keep seeing the *last
/// success*, not be reset by a failure (a tenant whose backups keep failing
/// must trip the alert, not look fresh).
pub async fn record_backup_failure(
    control: &ControlPlane,
    account_id: &str,
    db_name: &str,
    error: &str,
) -> Result<(), CloudError> {
    let bounded: String = error
        .chars()
        .take(crate::backup::DUMP_STDERR_MAX_LEN)
        .collect();
    sqlx::query(
        "UPDATE account_databases SET last_backup_error = $3 \
         WHERE account_id = $1 AND db_name = $2",
    )
    .bind(account_id)
    .bind(db_name)
    .bind(&bounded)
    .execute(control.pool())
    .await
    .map_err(CloudError::db("recording backup failure"))?;
    Ok(())
}

/// Insert this pass's `backup_runs` row (`kind` = `'nightly'` | `'final'`)
/// and return its id.
pub async fn start_backup_run(control: &ControlPlane, kind: &str) -> Result<String, CloudError> {
    let run_id = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO backup_runs (id, kind) VALUES ($1, $2)")
        .bind(&run_id)
        .bind(kind)
        .execute(control.pool())
        .await
        .map_err(CloudError::db("recording backup-run start"))?;
    Ok(run_id)
}

/// Finish a `backup_runs` row with its counts and finish timestamp.
pub async fn finish_backup_run(
    control: &ControlPlane,
    run_id: &str,
    total: usize,
    succeeded: usize,
    failed: usize,
) -> Result<(), CloudError> {
    sqlx::query(
        "UPDATE backup_runs \
         SET finished_at = NOW(), total = $2, succeeded = $3, failed = $4 \
         WHERE id = $1",
    )
    .bind(run_id)
    .bind(total as i32)
    .bind(succeeded as i32)
    .bind(failed as i32)
    .execute(control.pool())
    .await
    .map_err(CloudError::db("recording backup-run outcome"))?;
    Ok(())
}

/// Active tenants whose last successful backup is older than `horizon` (or
/// who have never been backed up *and* are older than the horizon — a tenant
/// provisioned minutes ago hasn't missed its nightly window yet). The
/// staleness monitor's alert query (plan: ">36h old").
pub async fn stale_tenant_backups(
    control: &ControlPlane,
    horizon: Duration,
    now: DateTime<Utc>,
) -> Result<Vec<StaleBackup>, CloudError> {
    let cutoff = now
        .checked_sub_signed(chrono::Duration::from_std(horizon).unwrap_or(chrono::Duration::MAX))
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    sqlx::query_as(
        "SELECT ad.account_id, ad.db_name, ad.last_backup_at \
         FROM account_databases ad \
         JOIN accounts a ON a.id = ad.account_id \
         WHERE ad.status = 'active' AND a.status = 'active' \
           AND a.created_at < $1 \
           AND (ad.last_backup_at IS NULL OR ad.last_backup_at < $1) \
         ORDER BY ad.last_backup_at ASC NULLS FIRST",
    )
    .bind(cutoff)
    .fetch_all(control.pool())
    .await
    .map_err(CloudError::db("listing stale tenant backups"))
}

// ==================== The nightly pass ====================

/// Run one nightly backup pass: dump every active tenant database (each under
/// its own per-account advisory lock) plus the control plane to `store`, and
/// record one `backup_runs` row. Never fails as a whole — per-target failures
/// land in [`BackupSummary::errors`] and on the tenant's row, and the rest of
/// the fleet proceeds (a broken tenant must not starve its neighbors).
///
/// Cross-pod safe by the reaper's mechanism: a tenant contended by another
/// pod's pass is skipped (observable, never waited on), and the control dump
/// is idempotent (same key per day; last writer wins).
pub async fn run_backup_pass(
    control: &ControlPlane,
    cluster: &ClusterConfig,
    store: &Arc<dyn BackupStore>,
    config: &BackupConfig,
    now: DateTime<Utc>,
) -> BackupSummary {
    let mut summary = BackupSummary::default();
    let run_id = match start_backup_run(control, "nightly").await {
        Ok(id) => Some(id),
        Err(e) => {
            // The ledger row is observability, not correctness — a failed
            // insert is recorded but the pass proceeds (the dumps are the
            // point).
            tracing::warn!(error = %e, "backup pass: recording run start failed");
            summary.errors.push(format!("recording run start: {e}"));
            None
        }
    };

    let conn = match DumpConnection::for_cluster(cluster) {
        Ok(conn) => conn,
        Err(e) => {
            tracing::error!(error = %e, "backup pass: cluster URL unusable; aborting pass");
            summary.errors.push(format!("cluster connection: {e}"));
            if let Some(run_id) = &run_id {
                let _ = finish_backup_run(control, run_id, 0, 0, 0).await;
            }
            return summary;
        }
    };

    let targets = match list_active_tenant_databases(control).await {
        Ok(targets) => targets,
        Err(e) => {
            tracing::error!(error = %e, "backup pass: listing tenants failed; aborting pass");
            summary.errors.push(format!("listing tenants: {e}"));
            if let Some(run_id) = &run_id {
                let _ = finish_backup_run(control, run_id, 0, 0, 0).await;
            }
            return summary;
        }
    };

    let mut backed_up = 0usize;
    for target in &targets {
        if backed_up >= config.max_backups_per_pass {
            summary.tenants_deferred.push(target.account_id.clone());
            continue;
        }
        match back_up_one_tenant(control, &conn, store, target, now).await {
            TenantBackupOutcome::Done => {
                backed_up += 1;
                summary.tenants_backed_up.push(target.account_id.clone());
            }
            TenantBackupOutcome::SkippedLocked => {
                summary
                    .tenants_skipped_locked
                    .push(target.account_id.clone());
            }
            TenantBackupOutcome::Failed(e) => {
                // A failed dump still consumed real time; count it against
                // the cap so a fleet of broken tenants can't spin the pass.
                backed_up += 1;
                tracing::warn!(
                    account_id = target.account_id,
                    db_name = target.db_name,
                    error = %e,
                    "backup pass: tenant dump failed"
                );
                summary.errors.push(format!(
                    "tenant {} ({}): {e}",
                    target.account_id, target.db_name
                ));
                summary.tenants_failed.push(target.account_id.clone());
                if let Err(rec) = record_backup_failure(
                    control,
                    &target.account_id,
                    &target.db_name,
                    &e.to_string(),
                )
                .await
                {
                    tracing::warn!(error = %rec, "backup pass: recording tenant failure failed");
                }
            }
        }
    }

    // Control-plane dump (plan: "plus the control plane"). One per pass, no
    // lock — the key is per-day and the dump is idempotent.
    match back_up_control_plane(control, &conn, store, now).await {
        Ok(()) => summary.control_backed_up = true,
        Err(e) => {
            tracing::error!(error = %e, "backup pass: control-plane dump failed");
            summary.errors.push(format!("control plane: {e}"));
        }
    }

    if let Some(run_id) = &run_id {
        let total = targets.len();
        let failed = summary.tenants_failed.len();
        let succeeded = summary.tenants_backed_up.len();
        if let Err(e) = finish_backup_run(control, run_id, total, succeeded, failed).await {
            tracing::warn!(error = %e, "backup pass: recording run outcome failed");
        }
    }

    summary
}

/// Per-tenant outcome inside the pass.
enum TenantBackupOutcome {
    Done,
    SkippedLocked,
    Failed(CloudError),
}

/// Dump one tenant under its per-account advisory lock, upload it, and record
/// success. The lock is the reaper's exact mechanism — a session-level
/// `pg_try_advisory_lock` keyed by the account id on a detached connection —
/// so two pods never dump the same tenant simultaneously, and the lock can't
/// strand on a dropped future.
async fn back_up_one_tenant(
    control: &ControlPlane,
    conn: &DumpConnection,
    store: &Arc<dyn BackupStore>,
    target: &BackupTarget,
    now: DateTime<Utc>,
) -> TenantBackupOutcome {
    let lock_conn = match try_account_advisory_lock(control, &target.account_id).await {
        Ok(Some(conn)) => conn,
        Ok(None) => return TenantBackupOutcome::SkippedLocked,
        Err(e) => return TenantBackupOutcome::Failed(e),
    };

    let result = async {
        let dump = dump_tenant_database(conn, &target.db_name).await?;
        let key = nightly_tenant_key(now, &target.db_name);
        store.put(&key, dump).await?;
        record_backup_success(control, &target.account_id, &target.db_name, now).await?;
        Ok::<(), CloudError>(())
    }
    .await;

    let _ = lock_conn.close().await;
    match result {
        Ok(()) => {
            tracing::info!(
                account_id = target.account_id,
                db_name = target.db_name,
                "backed up tenant database"
            );
            TenantBackupOutcome::Done
        }
        Err(e) => TenantBackupOutcome::Failed(e),
    }
}

/// Dump the control-plane database and upload it under the day's control key.
///
/// Uses the **cluster** connection params (`conn`) with the control database's
/// name. This is correct because v1 runs the control plane as "a dedicated
/// database on the shared cluster" (plan: "Tenant model") — same host, port,
/// and role as the tenant databases, differing only in the database name. If a
/// future deployment splits the control plane onto its own host/credentials,
/// this is the one call site that must derive its connection from the control
/// URL instead.
async fn back_up_control_plane(
    control: &ControlPlane,
    conn: &DumpConnection,
    store: &Arc<dyn BackupStore>,
    now: DateTime<Utc>,
) -> Result<(), CloudError> {
    let control_db = control_db_name(control)?;
    let dump = dump_control_database(conn, &control_db).await?;
    let key = nightly_control_key(now);
    store.put(&key, dump).await?;
    tracing::info!(control_db, "backed up control-plane database");
    Ok(())
}

/// The control-plane database name to dump. The pool's connect options carry
/// it; fall back to the default name when (improbably) absent.
fn control_db_name(control: &ControlPlane) -> Result<String, CloudError> {
    Ok(control
        .pool()
        .connect_options()
        .get_database()
        .unwrap_or(DEFAULT_CONTROL_DB_NAME)
        .to_string())
}

// ==================== Final dump (deletion path) ====================

/// Take the final logical dump of an account's tenant database **before** it
/// is dropped (plan: "Account deletion" step 4; "Backups & DR" → "Final dump
/// on account deletion"). The operator's only undo under hard-delete v1.
///
/// Scoped to the **active-account** deletion path by its callers: the reaper's
/// stuck-provision rollback and orphan-database reclaim drop never-activated
/// tenants (no real user data, possibly not even dumpable) and deliberately
/// do **not** call this — only [`delete_account`](crate::provision::delete_account)'s
/// own pre-drop hook and the interrupted-deletion arm (which completes a
/// deletion of an account that *was* active) reach it.
///
/// Fail-closed in the deletion sequence: a dump failure propagates so the
/// caller can abort the drop rather than destroy un-backed-up data. A tenant
/// database that's already gone (a retried deletion past the drop) is the one
/// tolerated case — handled by the caller checking existence, not here.
///
/// Records a `final`-kind `backup_runs` row for history. Returns the object
/// key the dump landed at.
pub async fn final_dump_before_delete(
    control: &ControlPlane,
    cluster: &ClusterConfig,
    store: &Arc<dyn BackupStore>,
    account_id: &str,
    db_name: &str,
    now: DateTime<Utc>,
) -> Result<String, CloudError> {
    let run_id = start_backup_run(control, "final").await.ok();

    let conn = DumpConnection::for_cluster(cluster)?;
    let result = async {
        let dump = dump_tenant_database(&conn, db_name).await?;
        let key = final_key(account_id, now);
        store.put(&key, dump).await?;
        Ok::<String, CloudError>(key)
    }
    .await;

    if let Some(run_id) = &run_id {
        match &result {
            Ok(_) => {
                let _ = finish_backup_run(control, run_id, 1, 1, 0).await;
            }
            Err(_) => {
                let _ = finish_backup_run(control, run_id, 1, 0, 1).await;
            }
        }
    }

    match result {
        Ok(key) => {
            tracing::info!(
                account_id,
                db_name,
                key,
                "took final backup before account deletion"
            );
            Ok(key)
        }
        Err(e) => Err(e),
    }
}

/// Whether a tenant database currently exists on the cluster — the
/// deletion-path guard that lets a retried deletion (past the drop) skip the
/// final dump instead of failing on a missing database. Shape-validates the
/// name first.
pub async fn tenant_database_exists(
    cluster: &ClusterConfig,
    db_name: &str,
) -> Result<bool, CloudError> {
    if !is_tenant_db_name(db_name) {
        return Err(CloudError::InvalidDatabaseName(db_name.to_string()));
    }
    let mut conn = cluster.connect_maintenance().await?;
    let exists = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)")
        .bind(db_name)
        .fetch_one(&mut conn)
        .await
        .map_err(CloudError::db(
            "checking tenant database existence for final dump",
        ));
    let _ = conn.close().await;
    exists
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_deterministic_and_dated() {
        let date = DateTime::parse_from_rfc3339("2026-06-09T03:14:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let db = "acct_aaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert_eq!(
            nightly_tenant_key(date, db),
            "backups/2026-06-09/acct_aaaaaaaaaaaaaaaaaaaaaaaaaa.dump"
        );
        assert_eq!(nightly_control_key(date), "backups/2026-06-09/control.dump");
        assert_eq!(
            final_key("11111111-2222-3333-4444-555555555555", date),
            "backups/final/11111111-2222-3333-4444-555555555555-20260609T031400Z.dump"
        );
    }

    #[test]
    fn empty_summary_is_quiet_any_action_is_not() {
        assert!(BackupSummary::default().is_quiet());
        let acted = BackupSummary {
            control_backed_up: true,
            ..BackupSummary::default()
        };
        assert!(!acted.is_quiet());
        let skipped = BackupSummary {
            tenants_skipped_locked: vec!["id".into()],
            ..BackupSummary::default()
        };
        assert!(!skipped.is_quiet());
    }
}
