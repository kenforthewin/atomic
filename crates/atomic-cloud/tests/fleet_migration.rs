//! Migration-tracking query-surface integration tests (plan: "Schema
//! migration on deploy"): the enumerate/record machinery the fleet runner
//! and the reaper's failed-migrations arm drive.
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command.

mod support;

use atomic_cloud::{
    list_retryable_failures, list_unmigrated, record_migration_failure, record_migration_success,
    tenant_schema_target, ControlPlane, MIGRATION_ERROR_MAX_LEN,
};
use chrono::{Duration, Utc};
use support::with_control_db;

/// Migrated control plane.
async fn setup(control_url: &str) -> ControlPlane {
    let control = ControlPlane::connect(control_url)
        .await
        .expect("connect control plane");
    control.initialize().await.expect("migrate control plane");
    control
}

/// Insert an account plus a mapping row at `version` with the given mapping
/// status. The db_name references no real database — these tests exercise
/// the control-plane bookkeeping only.
async fn seed_tenant(
    control: &ControlPlane,
    id: &str,
    db_name_char: &str,
    version: i32,
    db_status: &str,
) -> String {
    sqlx::query(
        "INSERT INTO accounts (id, subdomain, email, status, plan) \
         VALUES ($1, $1, 'k@example.com', 'active', 'free')",
    )
    .bind(id)
    .execute(control.pool())
    .await
    .expect("insert account");
    let db_name = format!("acct_{}", db_name_char.repeat(26));
    sqlx::query(
        "INSERT INTO account_databases \
             (account_id, cluster_id, db_name, status, last_migrated_version) \
         VALUES ($1, 'c1', $2, $3, $4)",
    )
    .bind(id)
    .bind(&db_name)
    .bind(db_status)
    .bind(version)
    .execute(control.pool())
    .await
    .expect("insert mapping row");
    db_name
}

#[tokio::test]
async fn enumerate_and_record_machinery() {
    with_control_db("enumerate_and_record_machinery", |url| async move {
        let control = setup(&url).await;
        let target = tenant_schema_target();
        assert!(target > 3, "tests below assume a multi-version registry");

        // far-behind and slightly-behind lag; current and retired don't.
        let far_db = seed_tenant(&control, "far-behind", "b", target - 3, "active").await;
        let near_db = seed_tenant(&control, "near-behind", "c", target - 1, "active").await;
        seed_tenant(&control, "current", "d", target, "active").await;
        seed_tenant(&control, "retired", "e", 0, "retired").await;

        // Plan step 1: only active, lagging rows — furthest behind first.
        let pending = list_unmigrated(&control, target).await.expect("list");
        let ids: Vec<(&str, i32)> = pending
            .iter()
            .map(|t| (t.account_id.as_str(), t.last_migrated_version))
            .collect();
        assert_eq!(
            ids,
            vec![("far-behind", target - 3), ("near-behind", target - 1)],
            "active lagging rows only, oldest version first"
        );
        assert_eq!(pending[0].db_name, far_db);
        assert_eq!(pending[0].migration_retry_count, 0);
        assert!(pending[0].migration_failed_at.is_none());

        // Success recording clears the row out of the pending set.
        record_migration_success(&control, "near-behind", &near_db, target)
            .await
            .expect("record success");
        let pending = list_unmigrated(&control, target).await.expect("list");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].account_id, "far-behind");

        // Failure recording: bounded error, failure time, backoff horizon,
        // bumped retry count — and the version untouched (the tenant is
        // exactly as migrated as before the attempt).
        let retry_after = Utc::now() + Duration::minutes(5);
        let oversized = "x".repeat(MIGRATION_ERROR_MAX_LEN + 200);
        record_migration_failure(
            &control,
            "far-behind",
            &far_db,
            &oversized,
            retry_after,
            target,
        )
        .await
        .expect("record failure");
        record_migration_failure(
            &control,
            "far-behind",
            &far_db,
            "second failure",
            retry_after,
            target,
        )
        .await
        .expect("record second failure");

        let pending = list_unmigrated(&control, target).await.expect("list");
        assert_eq!(pending.len(), 1, "a failed row remains pending");
        let failed = &pending[0];
        assert_eq!(failed.last_migrated_version, target - 3);
        assert!(failed.migration_failed_at.is_some());
        assert_eq!(
            failed.migration_retry_count, 2,
            "each failure bumps the count"
        );
        let horizon = failed.migration_retry_after.expect("backoff horizon set");
        assert!((horizon - retry_after).num_seconds().abs() < 1);
        let stored_error: String = sqlx::query_scalar(
            "SELECT last_migration_error FROM account_databases WHERE account_id = 'far-behind'",
        )
        .fetch_one(control.pool())
        .await
        .expect("read stored error");
        assert_eq!(stored_error, "second failure");

        // The oversized first error was bounded before storage — prove it
        // via the count the truncation guarantees on the re-recorded one.
        record_migration_failure(
            &control,
            "far-behind",
            &far_db,
            &oversized,
            retry_after,
            target,
        )
        .await
        .expect("record oversized failure");
        let stored_error: String = sqlx::query_scalar(
            "SELECT last_migration_error FROM account_databases WHERE account_id = 'far-behind'",
        )
        .fetch_one(control.pool())
        .await
        .expect("read stored error");
        assert_eq!(stored_error.chars().count(), MIGRATION_ERROR_MAX_LEN);

        // Success after failures: pending set empties, failure state clears.
        record_migration_success(&control, "far-behind", &far_db, target)
            .await
            .expect("record success after failures");
        assert!(
            list_unmigrated(&control, target)
                .await
                .expect("list")
                .is_empty(),
            "everything current"
        );
        type FailureState = (i32, Option<chrono::DateTime<Utc>>, Option<String>, i32);
        let (version, failed_at, error, retries): FailureState = sqlx::query_as(
            "SELECT last_migrated_version, migration_failed_at, last_migration_error, \
                    migration_retry_count \
             FROM account_databases WHERE account_id = 'far-behind'",
        )
        .fetch_one(control.pool())
        .await
        .expect("read recovered row");
        assert_eq!(version, target);
        assert!(failed_at.is_none());
        assert!(error.is_none());
        assert_eq!(retries, 0);

        // Monotone stamps: success with a LOWER version (an old binary in a
        // rolling deploy) never regresses the recorded one.
        record_migration_success(
            &control,
            "current",
            &format!("acct_{}", "d".repeat(26)),
            target - 2,
        )
        .await
        .expect("record stale success");
        let version: i32 = sqlx::query_scalar(
            "SELECT last_migrated_version FROM account_databases WHERE account_id = 'current'",
        )
        .fetch_one(control.pool())
        .await
        .expect("read current row");
        assert_eq!(version, target, "a stale success never regresses the stamp");
    })
    .await;
}

/// The reaper's worklist owns every lagging row, failure state or not
/// (the deploy-gating hardening): a never-attempted lagging row (NULL
/// horizon) is due immediately and listed first, a failed-and-due row
/// follows, a failed row inside its backoff horizon waits, and current or
/// retired rows are never listed. And the failure-recording guard: a stale
/// failure recording against an already-current row writes nothing.
#[tokio::test]
async fn retry_worklist_owns_all_lagging_rows() {
    with_control_db("retry_worklist_owns_all_lagging_rows", |url| async move {
        let control = setup(&url).await;
        let target = tenant_schema_target();
        let due_horizon = Utc::now() - Duration::minutes(1);
        let future_horizon = Utc::now() + Duration::hours(1);

        // Lagging, never attempted: no failure state at all.
        let unattempted_db = seed_tenant(&control, "unattempted", "f", target - 1, "active").await;
        // Lagging with failure state, backoff horizon passed.
        let failed_db = seed_tenant(&control, "failed-due", "g", target - 1, "active").await;
        record_migration_failure(
            &control,
            "failed-due",
            &failed_db,
            "boom",
            due_horizon,
            target,
        )
        .await
        .expect("record due failure");
        // Lagging with failure state, still backing off.
        let waiting_db = seed_tenant(&control, "backing-off", "h", target - 1, "active").await;
        record_migration_failure(
            &control,
            "backing-off",
            &waiting_db,
            "boom",
            future_horizon,
            target,
        )
        .await
        .expect("record backing-off failure");
        // Current and retired: never the reaper's business.
        let current_db = seed_tenant(&control, "current", "i", target, "active").await;
        seed_tenant(&control, "retired", "j", 0, "retired").await;

        let due = list_retryable_failures(&control, target)
            .await
            .expect("list retryable");
        let ids: Vec<&str> = due.iter().map(|t| t.account_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["unattempted", "failed-due"],
            "all lagging-and-due rows, never-attempted first (NULLS FIRST)"
        );
        assert_eq!(due[0].db_name, unattempted_db);
        assert!(due[0].migration_failed_at.is_none());

        // The stale-failure guard: a failure recording against a row
        // already stamped at the target (a concurrent success won) is
        // dropped — no permanent lie in the triage view.
        record_migration_failure(
            &control,
            "current",
            &current_db,
            "stale loser-pod connect error",
            due_horizon,
            target,
        )
        .await
        .expect("stale recording is a no-op, not an error");
        let (failed_at, retries): (Option<chrono::DateTime<Utc>>, i32) = sqlx::query_as(
            "SELECT migration_failed_at, migration_retry_count \
             FROM account_databases WHERE account_id = 'current'",
        )
        .fetch_one(control.pool())
        .await
        .expect("read current row");
        assert!(failed_at.is_none(), "no failure state on a current row");
        assert_eq!(retries, 0);
    })
    .await;
}
