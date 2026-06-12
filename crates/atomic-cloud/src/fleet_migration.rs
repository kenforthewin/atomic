//! Per-tenant schema-migration tracking and the boot-time fleet runner
//! (plan: "Provisioning lifecycle" → "Schema migration on deploy", steps
//! 1-5 + "Stragglers" + "Multi-pod boot").
//!
//! One tenant = one Postgres database, all running atomic-core's tenant
//! migrations — so a binary upgrade is a *fleet* migration: every tenant
//! database must be brought to the new binary's compiled schema target.
//! Migration 008 adds the tracking columns to `account_databases`; this
//! module holds the typed query surface over them plus the
//! [`FleetMigrator`] that drives them at boot, shared by three consumers:
//!
//! - **The boot-time fleet runner** ([`FleetMigrator`]) enumerates lagging
//!   tenants ([`list_unmigrated`]), fans out under a concurrency cap, runs
//!   `storage.initialize()` per tenant, and records each outcome
//!   ([`record_migration_success`] / [`record_migration_failure`]). The
//!   deploy gate around it (readiness, the failure-rate policy, the
//!   `deploy_runs` history) lives in [`crate::deploy`].
//! - **The reaper's failed-migrations arm** ([`crate::reaper`], arm 4)
//!   retries rows whose `migration_retry_after` has passed
//!   ([`list_retryable_failures`]) through the same per-tenant step and
//!   record functions ([`migrate_tenant`]).
//! - **CloudAuth's straggler gate** reads `last_migrated_version` on its
//!   per-request account lookup and returns the structured 503
//!   `account_upgrading` while a tenant lags [`tenant_schema_target`] (see
//!   `crate::auth`).
//!
//! [`provision_account`](crate::provision::provision_account) stamps the
//! compiled target when it writes the `account_databases` row (the tenant
//! was fully migrated two steps earlier), so fresh tenants are never
//! stragglers.
//!
//! # Multi-pod boot (plan: "Multi-pod boot")
//!
//! Every pod boots a [`FleetMigrator`] and races over the same fleet — no
//! coordination, no leader election. **The per-database advisory lock
//! inside atomic-core's migration runner (`storage.initialize()`) is the
//! multi-pod story**: two pods migrating the same tenant serialize on that
//! lock, the loser re-reads `schema_version` under it and applies nothing,
//! and both record the same success (monotonically — see
//! [`record_migration_success`]). Racing is safe, merely wasteful; the plan
//! defers a single-pod-claims-the-run optimization until deploy times hurt.

use std::collections::VecDeque;
use std::time::Duration;

use atomic_core::storage::{PgPoolConfig, PostgresStorage};
use chrono::{DateTime, Utc};
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::control_plane::ControlPlane;
use crate::error::CloudError;
use crate::provision::ClusterConfig;

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

/// The reaper's failed-migrations worklist (plan: "Failure recovery & the
/// reaper": `account_databases WHERE migration_failed_at IS NOT NULL AND
/// (migration_retry_after IS NULL OR migration_retry_after <= now())`):
/// active rows with recorded failure state whose backoff horizon has passed
/// (or was never written), oldest failure first — the tenants that have
/// been broken longest get retried soonest.
///
/// Deliberately *no* version filter: a row can carry failure state while
/// its stamp is already current (a concurrent pod's success raced this
/// pod's failure recording). Retrying it is an idempotent no-op
/// `initialize()` whose success recording clears the stale failure state —
/// self-healing, where a version filter would strand it forever. And
/// conversely, a lagging row with **no** failure state is *not* this list's
/// business — unattempted tenants belong to the boot runner
/// ([`list_unmigrated`]), not the reaper.
pub async fn list_retryable_failures(
    control: &ControlPlane,
) -> Result<Vec<UnmigratedTenant>, CloudError> {
    sqlx::query_as(
        "SELECT account_id, db_name, last_migrated_version, migration_failed_at, \
                migration_retry_after, migration_retry_count \
         FROM account_databases \
         WHERE status = 'active' AND migration_failed_at IS NOT NULL \
           AND (migration_retry_after IS NULL OR migration_retry_after <= NOW()) \
         ORDER BY migration_failed_at ASC, account_id ASC",
    )
    .fetch_all(control.pool())
    .await
    .map_err(CloudError::db("listing retryable failed migrations"))
}

/// An `account_databases` row carrying recorded migration-failure state —
/// the operator's triage view (`atomic-cloud deploy status`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FailedTenantMigration {
    pub account_id: String,
    pub db_name: String,
    pub last_migrated_version: i32,
    pub migration_failed_at: DateTime<Utc>,
    pub last_migration_error: Option<String>,
    pub migration_retry_after: Option<DateTime<Utc>>,
    pub migration_retry_count: i32,
}

/// Every active mapping row with recorded failure state, most-retried first
/// (the rows most likely to need a human). The reaper clears a row's
/// failure state on its next successful retry, so this list is exactly the
/// tenants still broken *right now*.
pub async fn list_failed_migrations(
    control: &ControlPlane,
) -> Result<Vec<FailedTenantMigration>, CloudError> {
    sqlx::query_as(
        "SELECT account_id, db_name, last_migrated_version, migration_failed_at, \
                last_migration_error, migration_retry_after, migration_retry_count \
         FROM account_databases \
         WHERE status = 'active' AND migration_failed_at IS NOT NULL \
         ORDER BY migration_retry_count DESC, migration_failed_at ASC",
    )
    .fetch_all(control.pool())
    .await
    .map_err(CloudError::db("listing failed tenant migrations"))
}

/// Tunables for one fleet migration run. [`Default`] is the production
/// configuration (plan numbers where the plan fixes one); the run-shaping
/// fields are `serve` CLI flags, and tests shrink them to drive specific
/// outcomes.
#[derive(Debug, Clone)]
pub struct FleetMigrationConfig {
    /// Tenants migrating concurrently (plan step 2: "start at 16, tune from
    /// production").
    pub concurrency: usize,

    /// Ceiling on establishing a tenant's migration connection — an
    /// unreachable tenant database must fail *recorded* (and quickly), not
    /// hold a fan-out slot for TCP's own timeout.
    pub tenant_connect_timeout: Duration,

    /// Wall-clock limit on the whole run (plan policy table: "Migration
    /// runs > 30 min" → `migration_timeout`). On expiry, in-flight
    /// migrations are abandoned (atomic-core's per-statement work either
    /// commits or it doesn't — `schema_version` stays consistent) and
    /// unattempted tenants stay enumerated for the next run.
    pub wall_clock_limit: Duration,

    /// Base of the failure-retry backoff horizon written to
    /// `migration_retry_after`: `base * 2^retry_count`, capped below. The
    /// always-running reaper retries rows whose horizon has passed.
    pub retry_backoff_base: Duration,

    /// Ceiling on the failure-retry backoff horizon.
    pub retry_backoff_cap: Duration,
}

impl Default for FleetMigrationConfig {
    fn default() -> Self {
        Self {
            concurrency: 16,
            tenant_connect_timeout: Duration::from_secs(10),
            wall_clock_limit: Duration::from_secs(30 * 60),
            retry_backoff_base: Duration::from_secs(60),
            retry_backoff_cap: Duration::from_secs(30 * 60),
        }
    }
}

/// What one fleet migration run did — the input to the deploy gate's
/// failure-rate policy ([`crate::deploy::evaluate_policy`]) and to the
/// `deploy_runs` history row.
#[derive(Debug, Clone)]
pub struct FleetRunOutcome {
    /// The compiled tenant schema target the run migrated toward.
    pub target: i32,
    /// Lagging tenants enumerated at the start of the run.
    pub total: usize,
    /// Tenants successfully migrated (and stamped) by this run — including
    /// tenants a concurrent pod migrated first, whose `initialize()` here
    /// was an idempotent no-op re-recording the same success.
    pub migrated: usize,
    /// Tenants whose migration failed; each has `migration_failed_at`,
    /// `last_migration_error`, and a `migration_retry_after` backoff
    /// recorded for the reaper.
    ///
    /// This deliberately also counts the rarer fault where the migration
    /// itself *succeeded* but the control-plane success recording failed
    /// (fail-closed: an unstamped tenant is still gated, so claiming it
    /// migrated would be a lie). A control plane flaky enough during boot
    /// to inflate this count into the review band is itself worth an
    /// operator's attention; the error-level log on each such recording
    /// failure is the discriminator.
    pub failed: usize,
    /// Whether the run hit [`FleetMigrationConfig::wall_clock_limit`].
    pub timed_out: bool,
    /// Wall-clock duration of the run.
    pub elapsed: Duration,
}

impl FleetRunOutcome {
    /// Tenants the run never got to (timeout abandoned them mid-queue or
    /// in flight). They stay enumerated for the reaper and the next boot.
    pub fn unattempted(&self) -> usize {
        self.total.saturating_sub(self.migrated + self.failed)
    }

    /// Recorded failures over enumerated tenants; `0.0` for an empty fleet
    /// (nothing lagged — vacuously healthy).
    pub fn failure_rate(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.failed as f64 / self.total as f64
        }
    }
}

/// The exponential backoff horizon recorded on a failed migration:
/// `now + base * 2^retry_count`, capped. `retry_count` is the row's count
/// *before* this failure (the recording UPDATE bumps it), so the first
/// failure gets the base horizon.
pub fn migration_backoff_horizon(retry_count: i32, base: Duration, cap: Duration) -> DateTime<Utc> {
    let exponent = retry_count.clamp(0, 30) as u32;
    let backoff = base.saturating_mul(2u32.saturating_pow(exponent)).min(cap);
    Utc::now()
        + chrono::Duration::from_std(backoff).unwrap_or_else(|_| chrono::Duration::seconds(60))
}

/// The boot-time fleet migration runner (plan steps 1-3; the module docs
/// cover the multi-pod story). Pure mechanism: it migrates and records, and
/// **never fails as a whole** — per-tenant failures are recorded and
/// counted, an unreadable control plane retries until the wall-clock limit,
/// and the worst outcome is `timed_out`. Policy (readiness, deploy status)
/// belongs to the caller ([`crate::deploy::run_fleet_gate`]).
pub struct FleetMigrator {
    control: ControlPlane,
    cluster: ClusterConfig,
    config: FleetMigrationConfig,
}

impl FleetMigrator {
    pub fn new(
        control: ControlPlane,
        cluster: ClusterConfig,
        config: FleetMigrationConfig,
    ) -> Self {
        Self {
            control,
            cluster,
            config,
        }
    }

    /// Run one fleet migration: enumerate, fan out, record. See
    /// [`FleetRunOutcome`] for what comes back.
    pub async fn run(&self) -> FleetRunOutcome {
        let started = Instant::now();
        let deadline = started + self.config.wall_clock_limit;
        let target = tenant_schema_target();

        // Plan step 1, retried within the budget: a transient control-plane
        // error at boot must not brick the pod's gate outright — but a
        // control plane unreadable for the whole wall-clock limit is a
        // timed-out run, honestly reported.
        let Some(pending) = self.enumerate_until(target, deadline).await else {
            return FleetRunOutcome {
                target,
                total: 0,
                migrated: 0,
                failed: 0,
                timed_out: true,
                elapsed: started.elapsed(),
            };
        };

        let total = pending.len();
        tracing::info!(
            target,
            total,
            concurrency = self.config.concurrency,
            "fleet migration: starting run over lagging tenants"
        );

        let mut pending: VecDeque<UnmigratedTenant> = pending.into();
        let mut join_set: JoinSet<bool> = JoinSet::new();
        let mut migrated = 0usize;
        let mut failed = 0usize;
        let mut timed_out = false;

        loop {
            while join_set.len() < self.config.concurrency.max(1) {
                let Some(tenant) = pending.pop_front() else {
                    break;
                };
                let control = self.control.clone();
                let cluster = self.cluster.clone();
                let config = self.config.clone();
                join_set.spawn(async move {
                    migrate_tenant(&control, &cluster, &config, tenant, target).await
                });
            }
            if join_set.is_empty() {
                break;
            }
            match tokio::time::timeout_at(deadline, join_set.join_next()).await {
                // Deadline hit with work still in flight: abandon it. The
                // aborted tasks' sessions close, releasing any held
                // migration advisory lock; nothing is recorded for them —
                // they are exactly as migrated as their last completed
                // statement, and stay enumerated for the next run.
                Err(_) => {
                    timed_out = true;
                    join_set.abort_all();
                    break;
                }
                Ok(Some(Ok(success))) => {
                    if success {
                        migrated += 1;
                    } else {
                        failed += 1;
                    }
                }
                Ok(Some(Err(join_error))) => {
                    // A panicked migration task is a bug, but the fleet run
                    // keeps the same never-abort contract as a recorded
                    // failure. Nothing was recorded for the tenant; it
                    // stays enumerated.
                    tracing::error!(error = %join_error, "fleet migration: tenant task panicked");
                    failed += 1;
                }
                Ok(None) => unreachable!("join_set checked non-empty"),
            }
        }

        FleetRunOutcome {
            target,
            total,
            migrated,
            failed,
            timed_out,
            elapsed: started.elapsed(),
        }
    }

    /// [`list_unmigrated`], retried (5s apart) until `deadline`. `None`
    /// means the control plane stayed unreadable for the whole budget.
    async fn enumerate_until(
        &self,
        target: i32,
        deadline: Instant,
    ) -> Option<Vec<UnmigratedTenant>> {
        loop {
            match list_unmigrated(&self.control, target).await {
                Ok(pending) => return Some(pending),
                Err(e) => {
                    tracing::warn!(error = %e, "fleet migration: enumerating lagging tenants failed; retrying");
                    let retry_at = Instant::now() + Duration::from_secs(5);
                    if retry_at >= deadline {
                        return None;
                    }
                    tokio::time::sleep_until(retry_at).await;
                }
            }
        }
    }
}

/// Plan step 3 for one tenant: connect a [`PostgresStorage`] to the tenant
/// database, run `initialize()` (atomic-core's advisory-locked migration
/// runner — concurrent pods serialize per tenant; an already-current schema
/// no-ops), and record the outcome. Returns `true` on recorded success.
///
/// A connect or migration failure records `migration_failed_at`,
/// `last_migration_error`, and the exponential `migration_retry_after`
/// horizon — and never aborts the fleet run. A failure *recording* failure
/// is logged (the tenant stays enumerated either way).
///
/// `pub(crate)`: the reaper's failed-migrations arm
/// ([`crate::reaper`], arm 4) retries [`list_retryable_failures`] rows
/// through this exact step, so a reaper retry and a boot-runner attempt are
/// indistinguishable in what they run and what they record. Of its
/// [`FleetMigrationConfig`] only the per-tenant fields matter here
/// (`tenant_connect_timeout`, `retry_backoff_base`, `retry_backoff_cap`);
/// the run-shaping fields belong to [`FleetMigrator::run`].
pub(crate) async fn migrate_tenant(
    control: &ControlPlane,
    cluster: &ClusterConfig,
    config: &FleetMigrationConfig,
    tenant: UnmigratedTenant,
    target: i32,
) -> bool {
    let result = run_tenant_migration(cluster, config, &tenant.db_name).await;

    match result {
        Ok(()) => {
            tracing::info!(
                account_id = tenant.account_id,
                db_name = tenant.db_name,
                from_version = tenant.last_migrated_version,
                to_version = target,
                "fleet migration: tenant migrated"
            );
            if let Err(e) =
                record_migration_success(control, &tenant.account_id, &tenant.db_name, target).await
            {
                tracing::error!(
                    account_id = tenant.account_id,
                    error = %e,
                    "fleet migration: success recording failed; the tenant is \
                     migrated but stays gated until a later run re-records it"
                );
                return false;
            }
            true
        }
        Err(e) => {
            let retry_after = migration_backoff_horizon(
                tenant.migration_retry_count,
                config.retry_backoff_base,
                config.retry_backoff_cap,
            );
            tracing::warn!(
                account_id = tenant.account_id,
                db_name = tenant.db_name,
                retry_count = tenant.migration_retry_count + 1,
                retry_after = %retry_after,
                error = %e,
                "fleet migration: tenant migration failed; recorded for the reaper"
            );
            if let Err(record_err) = record_migration_failure(
                control,
                &tenant.account_id,
                &tenant.db_name,
                &e.to_string(),
                retry_after,
            )
            .await
            {
                tracing::error!(
                    account_id = tenant.account_id,
                    error = %record_err,
                    "fleet migration: failure recording failed"
                );
            }
            false
        }
    }
}

/// Connect to one tenant database and bring its schema to the compiled
/// target. The single-connection pool exists only for this call and is
/// closed on every path; the connect itself is bounded by
/// [`FleetMigrationConfig::tenant_connect_timeout`] (sqlx's pool connect
/// acquires — and therefore establishes — one connection eagerly, so a
/// dropped database or unreachable host fails here, typed).
async fn run_tenant_migration(
    cluster: &ClusterConfig,
    config: &FleetMigrationConfig,
    db_name: &str,
) -> Result<(), CloudError> {
    let url = cluster.tenant_db_url(db_name)?;
    let storage = PostgresStorage::connect_with_config(
        &url,
        "default",
        PgPoolConfig {
            max_connections: 1,
            acquire_timeout: config.tenant_connect_timeout,
            idle_timeout: None,
            max_lifetime: None,
            slow_query_threshold: None,
        },
    )
    .await
    .map_err(CloudError::core(
        "connecting to tenant database for migration",
    ))?;
    let outcome = storage
        .initialize()
        .await
        .map_err(CloudError::core("running tenant migrations"));
    storage.pool().close().await;
    outcome
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

    /// The backoff doubles per prior failure from the base and saturates at
    /// the cap — including absurd retry counts (no overflow panic).
    #[test]
    fn migration_backoff_doubles_and_caps() {
        let base = Duration::from_secs(60);
        let cap = Duration::from_secs(1800);
        let horizon_secs = |count: i32| {
            let horizon = migration_backoff_horizon(count, base, cap);
            (horizon - Utc::now()).num_seconds()
        };
        // ±2s slack absorbs clock movement between the two Utc::now() reads.
        assert!((58..=62).contains(&horizon_secs(0)), "first failure: base");
        assert!((118..=122).contains(&horizon_secs(1)), "second: doubled");
        assert!((1798..=1802).contains(&horizon_secs(10)), "capped");
        assert!(
            (1798..=1802).contains(&horizon_secs(i32::MAX)),
            "no overflow"
        );
        assert!(
            (58..=62).contains(&horizon_secs(-5)),
            "a (theoretical) negative count clamps to the base horizon"
        );
    }

    #[test]
    fn outcome_rate_and_unattempted() {
        let outcome = FleetRunOutcome {
            target: 22,
            total: 20,
            migrated: 17,
            failed: 1,
            timed_out: true,
            elapsed: Duration::from_secs(1),
        };
        assert_eq!(outcome.unattempted(), 2);
        assert!((outcome.failure_rate() - 0.05).abs() < f64::EPSILON);

        let empty = FleetRunOutcome {
            target: 22,
            total: 0,
            migrated: 0,
            failed: 0,
            timed_out: false,
            elapsed: Duration::ZERO,
        };
        assert_eq!(
            empty.failure_rate(),
            0.0,
            "empty fleet is vacuously healthy"
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
