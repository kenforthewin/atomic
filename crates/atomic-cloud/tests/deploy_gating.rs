//! Deploy-gating integration tests (plan: "Provisioning lifecycle" →
//! "Schema migration on deploy"): the boot fleet migration
//! ([`FleetMigrator`]), the failure-rate policy, the `/ready` readiness
//! gate, `deploy advance`, and multi-pod racing.
//!
//! Failure injection is honest: a "broken" tenant is a mapping row whose
//! `db_name` points at a database that does not exist — a real connect
//! failure, not a mock. "Healthy but stale-stamped" tenants are real
//! databases (some empty, migrated for real by the run under test; some
//! already current, where `initialize()` no-ops idempotently and the run
//! re-records success — that *is* the correct behavior for an
//! already-current schema). Threshold crossings are manufactured by
//! stamping many mapping rows at one already-current database (distinct
//! `cluster_id` per row satisfies the `UNIQUE (cluster_id, db_name)`
//! constraint; v1's single-cluster `tenant_db_url` ignores `cluster_id`).
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command.

mod support;

use std::sync::Arc;
use std::time::Duration;

use actix_web::{App, HttpServer};
use atomic_cloud::{
    abandoned_run_threshold, advance_deploy, configure_cloud_app, finalize_abandoned_runs,
    latest_deploy_run, run_fleet_gate, tenant_db_name, tenant_schema_target, AccountCache,
    AccountCacheConfig, AccountPlane, AccountPlaneConfig, AdvanceOutcome, ChatStreamLimiter,
    CloudAuth, ClusterConfig, ControlPlane, DeployPolicy, FallbackAppState, FleetMigrationConfig,
    FleetMigrator, ManagedKeys, Readiness, TenantPlane, DEFAULT_CHAT_STREAMS_PER_ACCOUNT,
};
use atomic_core::storage::PostgresStorage;
use chrono::{DateTime, Utc};
use sqlx::{Connection, PgConnection};
use support::{create_database, with_control_db, CapturingSender};

const BASE_DOMAIN: &str = "cloudtest.local";

fn cluster_config() -> ClusterConfig {
    ClusterConfig {
        cluster_id: "test-cluster-1".to_string(),
        cluster_url: std::env::var("ATOMIC_TEST_DATABASE_URL")
            .expect("with_control_db verified ATOMIC_TEST_DATABASE_URL"),
    }
}

async fn connect_control(control_url: &str) -> ControlPlane {
    let control = ControlPlane::connect(control_url)
        .await
        .expect("connect control plane");
    control.initialize().await.expect("migrate control plane");
    control
}

/// Fast-but-realistic run tunables: small fan-out, generous wall clock (the
/// timeout test shrinks it), short connect timeout so the broken-tenant
/// failure path is quick.
fn fast_config() -> FleetMigrationConfig {
    FleetMigrationConfig {
        concurrency: 8,
        tenant_connect_timeout: Duration::from_secs(5),
        wall_clock_limit: Duration::from_secs(120),
        ..FleetMigrationConfig::default()
    }
}

/// Insert an account plus an active mapping row stamped at `version`.
/// Non-UUID account ids skip the cleanup sweep's name derivation, but every
/// `db_name` seeded here is recorded in `account_databases`, which the
/// sweep reads directly.
async fn seed_stamped_tenant(
    control: &ControlPlane,
    account_id: &str,
    cluster_id: &str,
    db_name: &str,
    version: i32,
) {
    sqlx::query(
        "INSERT INTO accounts (id, subdomain, email, status, plan) \
         VALUES ($1, $1, 'k@example.com', 'active', 'free')",
    )
    .bind(account_id)
    .execute(control.pool())
    .await
    .expect("insert account");
    sqlx::query(
        "INSERT INTO account_databases \
             (account_id, cluster_id, db_name, status, last_migrated_version) \
         VALUES ($1, $2, $3, 'active', $4)",
    )
    .bind(account_id)
    .bind(cluster_id)
    .bind(db_name)
    .bind(version)
    .execute(control.pool())
    .await
    .expect("insert mapping row");
}

/// Create one real, empty tenant database (tenant-shaped name so the
/// support sweep recognizes it).
async fn create_empty_tenant_db() -> String {
    let cluster = cluster_config();
    let db_name = tenant_db_name(uuid::Uuid::new_v4());
    create_database(&cluster.cluster_url, &db_name).await;
    db_name
}

/// Create one real tenant database and bring it fully current — the shared
/// target of the threshold tests' many stale-stamped mapping rows.
async fn create_current_tenant_db() -> String {
    let db_name = create_empty_tenant_db().await;
    let url = cluster_config()
        .tenant_db_url(&db_name)
        .expect("tenant url");
    let storage = PostgresStorage::connect(&url, "default")
        .await
        .expect("connect tenant db");
    storage.initialize().await.expect("migrate tenant db");
    storage.pool().close().await;
    db_name
}

/// One mapping row's migration-tracking state.
type RowState = (
    i32,                   // last_migrated_version
    Option<DateTime<Utc>>, // last_migrated_at
    Option<DateTime<Utc>>, // migration_failed_at
    Option<String>,        // last_migration_error
    Option<DateTime<Utc>>, // migration_retry_after
    i32,                   // migration_retry_count
);

async fn row_state(control: &ControlPlane, account_id: &str) -> RowState {
    sqlx::query_as(
        "SELECT last_migrated_version, last_migrated_at, migration_failed_at, \
                last_migration_error, migration_retry_after, migration_retry_count \
         FROM account_databases WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_one(control.pool())
    .await
    .expect("read mapping row state")
}

/// Drive one readiness probe and decode it.
async fn probe_json(readiness: &Readiness) -> (u16, serde_json::Value) {
    let resp = readiness.probe().await;
    let status = resp.status().as_u16();
    let body = actix_web::body::to_bytes(resp.into_body())
        .await
        .expect("read probe body");
    (
        status,
        serde_json::from_slice(&body).expect("probe body is json"),
    )
}

// ==================== The happy path ====================

/// Plan steps 1-5, success arm: a fleet of genuinely stale tenants (real,
/// empty databases stamped behind the target) is migrated for real, every
/// row is stamped at the compiled target, the `deploy_runs` row records the
/// counts, and readiness flips from migrating to ready.
#[tokio::test]
async fn fleet_run_migrates_stale_fleet_and_flips_ready() {
    with_control_db(
        "fleet_run_migrates_stale_fleet_and_flips_ready",
        |url| async move {
            let control = connect_control(&url).await;
            let target = tenant_schema_target();

            let mut tenants = Vec::new();
            for i in 0..3 {
                let db_name = create_empty_tenant_db().await;
                let account_id = format!("stale-{i}");
                seed_stamped_tenant(&control, &account_id, "c1", &db_name, 0).await;
                tenants.push((account_id, db_name));
            }

            let readiness = Readiness::new(control.clone());
            let (status, body) = probe_json(&readiness).await;
            assert_eq!(status, 503, "boot state is migrating: not ready");
            assert_eq!(body["status"], "migrating");
            assert!(body["since"].is_string());

            run_fleet_gate(
                control.clone(),
                cluster_config(),
                fast_config(),
                DeployPolicy::default(),
                readiness.clone(),
            )
            .await;

            assert!(readiness.is_ready().await, "0% failures must flip ready");
            let (status, body) = probe_json(&readiness).await;
            assert_eq!(status, 200);
            assert_eq!(body["status"], "ready");

            for (account_id, db_name) in &tenants {
                let (version, migrated_at, failed_at, error, retry_after, retries) =
                    row_state(&control, account_id).await;
                assert_eq!(version, target, "{account_id} stamped at the target");
                assert!(migrated_at.is_some(), "{account_id} has last_migrated_at");
                assert!(failed_at.is_none() && error.is_none(), "no failure state");
                assert!(retry_after.is_none() && retries == 0, "no backoff state");

                // The stamp is honest: the tenant database really carries
                // the full schema, not just a bookkeeping row.
                let tenant_url = cluster_config().tenant_db_url(db_name).expect("tenant url");
                let mut conn = PgConnection::connect(&tenant_url)
                    .await
                    .expect("connect migrated tenant");
                let version: i32 = sqlx::query_scalar("SELECT MAX(version) FROM schema_version")
                    .fetch_one(&mut conn)
                    .await
                    .expect("read tenant schema version");
                conn.close().await.expect("close");
                assert_eq!(version, target, "tenant schema is really at the target");
            }

            let run = latest_deploy_run(&control)
                .await
                .expect("read latest run")
                .expect("the gate recorded a run");
            assert_eq!(run.deploy_status, "ready");
            assert_eq!(run.target_version, target);
            assert!(run.finished_at.is_some());
            assert_eq!(
                (run.total, run.migrated, run.failed),
                (Some(3), Some(3), Some(0))
            );
        },
    )
    .await;
}

// ==================== The policy bands ====================

/// `0 < x < 1%`: one honestly broken tenant (its database does not exist)
/// among enough healthy rows that the rate stays under the ready threshold.
/// The failure is recorded — error, failure time, backoff horizon, bumped
/// retry count — the fleet run continues past it, and readiness still flips
/// ready: sub-threshold failures are stragglers (CloudAuth 503s them
/// per-request; the reaper retries them).
#[tokio::test]
async fn sub_threshold_failure_is_recorded_and_still_flips_ready() {
    with_control_db(
        "sub_threshold_failure_is_recorded_and_still_flips_ready",
        |url| async move {
            let control = connect_control(&url).await;
            let target = tenant_schema_target();

            // 110 healthy stale-stamped rows on one current database + 1
            // broken: 1/111 ≈ 0.9% < 1%.
            let shared_db = create_current_tenant_db().await;
            for i in 0..110 {
                seed_stamped_tenant(
                    &control,
                    &format!("healthy-{i}"),
                    &format!("c{i}"),
                    &shared_db,
                    target - 1,
                )
                .await;
            }
            let ghost_db = tenant_db_name(uuid::Uuid::new_v4());
            seed_stamped_tenant(&control, "broken", "c-broken", &ghost_db, target - 1).await;

            let readiness = Readiness::new(control.clone());
            run_fleet_gate(
                control.clone(),
                cluster_config(),
                fast_config(),
                DeployPolicy::default(),
                readiness.clone(),
            )
            .await;

            assert!(
                readiness.is_ready().await,
                "0.9% failure rate is below the ready threshold"
            );

            // The straggler's failure state, fully recorded.
            let (version, _, failed_at, error, retry_after, retries) =
                row_state(&control, "broken").await;
            assert_eq!(version, target - 1, "a failed tenant's stamp is untouched");
            assert!(failed_at.is_some(), "failure time recorded");
            assert!(
                !error.expect("error text recorded").is_empty(),
                "error text recorded"
            );
            assert!(
                retry_after.expect("backoff horizon recorded") > Utc::now(),
                "backoff horizon is in the future"
            );
            assert_eq!(retries, 1, "first failure bumps the retry count to 1");

            // The fleet continued past the failure: every healthy row got
            // re-recorded at the target (initialize() no-oped — the schema
            // was already current, which is exactly a recordable success).
            let lagging: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM account_databases \
                 WHERE status = 'active' AND last_migrated_version < $1",
            )
            .bind(target)
            .fetch_one(control.pool())
            .await
            .expect("count lagging");
            assert_eq!(lagging, 1, "only the broken tenant still lags");

            let run = latest_deploy_run(&control)
                .await
                .expect("read latest run")
                .expect("run recorded");
            assert_eq!(run.deploy_status, "ready");
            assert_eq!(
                (run.total, run.migrated, run.failed),
                (Some(111), Some(110), Some(1))
            );
        },
    )
    .await;
}

/// `1% ≤ x < 10%`: the pod holds not-ready with
/// `deploy_status='awaiting_review'` — and `deploy advance` (the operator
/// acknowledgment, recorded in the control plane so every pod sees it)
/// flips readiness through the probe's re-check, persisting `advanced` +
/// `advanced_at` on the run row.
#[tokio::test]
async fn review_band_holds_until_deploy_advance() {
    with_control_db("review_band_holds_until_deploy_advance", |url| async move {
        let control = connect_control(&url).await;
        let target = tenant_schema_target();

        // 19 healthy + 1 broken: 5% — inside the review band.
        let shared_db = create_current_tenant_db().await;
        for i in 0..19 {
            seed_stamped_tenant(
                &control,
                &format!("healthy-{i}"),
                &format!("c{i}"),
                &shared_db,
                target - 1,
            )
            .await;
        }
        let ghost_db = tenant_db_name(uuid::Uuid::new_v4());
        seed_stamped_tenant(&control, "broken", "c-broken", &ghost_db, target - 1).await;

        let readiness = Readiness::new(control.clone());
        run_fleet_gate(
            control.clone(),
            cluster_config(),
            fast_config(),
            DeployPolicy::default(),
            readiness.clone(),
        )
        .await;

        assert!(!readiness.is_ready().await, "review band holds not-ready");
        let (status, body) = probe_json(&readiness).await;
        assert_eq!(status, 503);
        assert_eq!(body["status"], "awaiting_review");
        let run_id = body["run_id"].as_str().expect("hold carries its run id");

        let run = latest_deploy_run(&control)
            .await
            .expect("read latest run")
            .expect("run recorded");
        assert_eq!(run.id, run_id);
        assert_eq!(run.deploy_status, "awaiting_review");
        assert_eq!(
            (run.total, run.migrated, run.failed),
            (Some(20), Some(19), Some(1))
        );

        // The operator acknowledges. The flip is durable (all pods would
        // observe it) and this pod picks it up on its next probe.
        match advance_deploy(&control).await.expect("advance") {
            AdvanceOutcome::Advanced {
                target_version,
                runs,
            } => {
                assert_eq!(target_version, target);
                assert_eq!(runs, 1);
            }
            other => panic!("expected Advanced, got {other:?}"),
        }

        let (status, body) = probe_json(&readiness).await;
        assert_eq!(status, 200, "the probe's re-check observes the advance");
        assert_eq!(body["status"], "ready");
        assert!(
            readiness.is_ready().await,
            "the flip is settled, not per-probe"
        );

        let run = latest_deploy_run(&control)
            .await
            .expect("read latest run")
            .expect("run recorded");
        assert_eq!(run.deploy_status, "advanced");
        assert!(run.advanced_at.is_some(), "the acknowledgment is persisted");

        // A second advance has nothing left to acknowledge.
        match advance_deploy(&control).await.expect("re-advance") {
            AdvanceOutcome::NothingToAdvance { status } => assert_eq!(status, "advanced"),
            other => panic!("expected NothingToAdvance, got {other:?}"),
        }
    })
    .await;
}

/// `x ≥ 10%`: `rollback_required` — the pod holds not-ready and `deploy
/// advance` refuses, by design: the migration itself is broken and the only
/// remedy is redeploying the old binary (safe under additive-only
/// migrations). There is deliberately no override to test.
#[tokio::test]
async fn rollback_band_holds_and_advance_refuses() {
    with_control_db(
        "rollback_band_holds_and_advance_refuses",
        |url| async move {
            let control = connect_control(&url).await;
            let target = tenant_schema_target();

            // 4 healthy + 1 broken: 20% — at/above the rollback threshold.
            let shared_db = create_current_tenant_db().await;
            for i in 0..4 {
                seed_stamped_tenant(
                    &control,
                    &format!("healthy-{i}"),
                    &format!("c{i}"),
                    &shared_db,
                    target - 1,
                )
                .await;
            }
            let ghost_db = tenant_db_name(uuid::Uuid::new_v4());
            seed_stamped_tenant(&control, "broken", "c-broken", &ghost_db, target - 1).await;

            let readiness = Readiness::new(control.clone());
            run_fleet_gate(
                control.clone(),
                cluster_config(),
                fast_config(),
                DeployPolicy::default(),
                readiness.clone(),
            )
            .await;

            assert!(!readiness.is_ready().await);
            let (status, body) = probe_json(&readiness).await;
            assert_eq!(status, 503);
            assert_eq!(body["status"], "rollback_required");

            assert!(
                matches!(
                    advance_deploy(&control).await.expect("advance attempt"),
                    AdvanceOutcome::RefusedRollbackRequired
                ),
                "rollback_required has no advance override"
            );
            assert!(
                !readiness.is_ready().await,
                "still holding after the refusal"
            );
            let run = latest_deploy_run(&control)
                .await
                .expect("read latest run")
                .expect("run recorded");
            assert_eq!(run.deploy_status, "rollback_required");
            assert!(run.advanced_at.is_none());
        },
    )
    .await;
}

/// Wall-clock policy: a wedged tenant (atomic-core's migration advisory
/// lock held by an outside session, so `initialize()` blocks forever) plus
/// a small wall-clock limit produces `migration_timeout` — the pod holds
/// not-ready, the wedged tenant is *unattempted* (no failure recorded, no
/// stamp moved), and nothing is advanceable. A healthy tenant migrated
/// before the deadline is still counted and persisted: a timed-out run's
/// `deploy_runs` row reports the partial work that really happened, not
/// zeros.
#[tokio::test]
async fn wall_clock_timeout_holds_with_migration_timeout() {
    with_control_db(
        "wall_clock_timeout_holds_with_migration_timeout",
        |url| async move {
            let control = connect_control(&url).await;
            let target = tenant_schema_target();

            let db_name = create_empty_tenant_db().await;
            seed_stamped_tenant(&control, "wedged", "c1", &db_name, 0).await;
            // A healthy stale-stamped tenant on an already-current database
            // migrates (a fast no-op) well inside the limit.
            let healthy_db = create_current_tenant_db().await;
            seed_stamped_tenant(&control, "healthy", "c2", &healthy_db, target - 1).await;

            // Hold atomic-core's migration advisory lock from a session the
            // runner can't see; its initialize() blocks acquiring it.
            // 0x61746f6d69635f6d ("atomic_m") is the runner's fixed lock key
            // (crates/atomic-core/src/storage/postgres/mod.rs).
            let tenant_url = cluster_config()
                .tenant_db_url(&db_name)
                .expect("tenant url");
            let mut wedge = PgConnection::connect(&tenant_url)
                .await
                .expect("connect wedge session");
            sqlx::query("SELECT pg_advisory_lock($1)")
                .bind(0x61746f6d69635f6du64 as i64)
                .execute(&mut wedge)
                .await
                .expect("take migration lock");

            let readiness = Readiness::new(control.clone());
            run_fleet_gate(
                control.clone(),
                cluster_config(),
                FleetMigrationConfig {
                    wall_clock_limit: Duration::from_secs(2),
                    ..fast_config()
                },
                DeployPolicy::default(),
                readiness.clone(),
            )
            .await;

            assert!(!readiness.is_ready().await, "timeout holds not-ready");
            let (status, body) = probe_json(&readiness).await;
            assert_eq!(status, 503);
            assert_eq!(body["status"], "migration_timeout");

            // The wedged tenant was abandoned, not failed: no failure state,
            // no stamp movement — it stays enumerated for the next run (and
            // for the reaper's lagging-row arm).
            let (version, _, failed_at, error, _, retries) = row_state(&control, "wedged").await;
            assert_eq!(version, 0);
            assert!(failed_at.is_none() && error.is_none() && retries == 0);
            // The healthy tenant finished inside the limit and is stamped.
            let (version, _, failed_at, _, _, _) = row_state(&control, "healthy").await;
            assert_eq!(version, target, "the healthy tenant migrated");
            assert!(failed_at.is_none());

            let run = latest_deploy_run(&control)
                .await
                .expect("read latest run")
                .expect("run recorded");
            assert_eq!(run.deploy_status, "migration_timeout");
            assert_eq!(
                (run.total, run.migrated, run.failed),
                (Some(2), Some(1), Some(0)),
                "partial counts persist on timeout: the healthy migration is \
                 counted, the wedged tenant is unattempted, not failed"
            );

            assert!(
                matches!(
                    advance_deploy(&control).await.expect("advance attempt"),
                    AdvanceOutcome::NothingToAdvance { .. }
                ),
                "a timeout is not reviewable; restart re-runs the fleet"
            );

            wedge.close().await.expect("close wedge session");
        },
    )
    .await;
}

// ==================== Multi-pod racing ====================

/// Plan "Multi-pod boot": two FleetMigrator instances race over the same
/// genuinely stale fleet with no coordination. atomic-core's per-database
/// advisory lock serializes them per tenant — the loser's `initialize()`
/// no-ops and re-records the same success — so both runs finish with zero
/// failures and every tenant ends migrated exactly-once-effectively.
#[tokio::test]
async fn concurrent_fleet_runs_converge_without_errors() {
    with_control_db(
        "concurrent_fleet_runs_converge_without_errors",
        |url| async move {
            let control = connect_control(&url).await;
            let target = tenant_schema_target();

            let mut tenants = Vec::new();
            for i in 0..3 {
                let db_name = create_empty_tenant_db().await;
                let account_id = format!("raced-{i}");
                seed_stamped_tenant(&control, &account_id, "c1", &db_name, 0).await;
                tenants.push((account_id, db_name));
            }

            let pod_a = FleetMigrator::new(control.clone(), cluster_config(), fast_config());
            let pod_b = FleetMigrator::new(control.clone(), cluster_config(), fast_config());
            let (a, b) = tokio::join!(pod_a.run(), pod_b.run());

            for (label, outcome) in [("pod A", &a), ("pod B", &b)] {
                assert!(!outcome.timed_out, "{label} must not time out");
                assert_eq!(
                    outcome.failed, 0,
                    "{label}: racing is safe, never a failure"
                );
                assert_eq!(
                    outcome.migrated, outcome.total,
                    "{label}: every tenant it enumerated ends recorded migrated"
                );
            }
            assert_eq!(
                a.total, 3,
                "the first enumeration (join! polls A first) sees the whole stale fleet"
            );

            for (account_id, db_name) in &tenants {
                let (version, _, failed_at, error, _, retries) =
                    row_state(&control, account_id).await;
                assert_eq!(version, target, "{account_id} stamped at the target");
                assert!(
                    failed_at.is_none() && error.is_none() && retries == 0,
                    "{account_id}: the race must record no failure state"
                );

                let tenant_url = cluster_config().tenant_db_url(db_name).expect("tenant url");
                let mut conn = PgConnection::connect(&tenant_url)
                    .await
                    .expect("connect raced tenant");
                // Exactly-once-effectively: no version row recorded twice —
                // racing pods serialized on the advisory lock instead of
                // double-applying. (Not every tenant migration records a
                // version row, so the check is duplicates, not count.)
                let (max_version, rows, distinct): (i32, i64, i64) = sqlx::query_as(
                    "SELECT MAX(version), COUNT(*), COUNT(DISTINCT version) FROM schema_version",
                )
                .fetch_one(&mut conn)
                .await
                .expect("read tenant schema versions");
                conn.close().await.expect("close");
                assert_eq!(max_version, target);
                assert_eq!(
                    rows, distinct,
                    "{account_id}: no migration version recorded twice"
                );
            }
        },
    )
    .await;
}

// ==================== Stale-run finalization ====================

/// A dead pod's stuck `migrating` row must not shadow a live review:
/// without finalization, `deploy advance` would answer NothingToAdvance
/// ('migrating') forever. `finalize_abandoned_runs` flips stale rows to the
/// terminal 'abandoned', `advance_deploy` skips abandoned rows — and a
/// *fresh* `migrating` row still shadows, conservatively, by design.
#[tokio::test]
async fn stale_migrating_run_cannot_shadow_advance() {
    with_control_db(
        "stale_migrating_run_cannot_shadow_advance",
        |url| async move {
            let control = connect_control(&url).await;
            let target = tenant_schema_target();
            let threshold = abandoned_run_threshold(&fast_config());

            // Pod A's review, then pod B's later row — B died mid-run hours
            // ago and its row is stuck 'migrating'.
            sqlx::query(
                "INSERT INTO deploy_runs (id, target_version, started_at, deploy_status) \
                 VALUES ('review-run', $1, NOW() - interval '4 hours', 'awaiting_review'), \
                        ('dead-run', $1, NOW() - interval '3 hours', 'migrating')",
            )
            .bind(target)
            .execute(control.pool())
            .await
            .expect("seed deploy runs");

            // The unfixed failure mode: the dead row shadows the review.
            match advance_deploy(&control).await.expect("advance attempt") {
                AdvanceOutcome::NothingToAdvance { status } => assert_eq!(status, "migrating"),
                other => panic!("expected NothingToAdvance, got {other:?}"),
            }

            // Finalization (run on boot and by `deploy status`/`advance`).
            let finalized = finalize_abandoned_runs(&control, threshold)
                .await
                .expect("finalize stale runs");
            assert_eq!(finalized, 1, "only the stale migrating row");
            assert_eq!(
                finalize_abandoned_runs(&control, threshold)
                    .await
                    .expect("re-finalize"),
                0,
                "idempotent"
            );

            // The abandoned row is terminal history...
            let (status, finished): (String, Option<DateTime<Utc>>) = sqlx::query_as(
                "SELECT deploy_status, finished_at FROM deploy_runs WHERE id = 'dead-run'",
            )
            .fetch_one(control.pool())
            .await
            .expect("read dead run");
            assert_eq!(status, "abandoned");
            assert!(finished.is_some(), "finalization stamps finished_at");

            // ...and cannot shadow: advance now finds and flips the review.
            match advance_deploy(&control).await.expect("advance") {
                AdvanceOutcome::Advanced {
                    target_version,
                    runs,
                } => {
                    assert_eq!(target_version, target);
                    assert_eq!(runs, 1);
                }
                other => panic!("expected Advanced, got {other:?}"),
            }

            // A FRESH migrating row (a pod genuinely mid-run) is neither
            // finalized nor skipped: the conservative gate holds.
            sqlx::query("INSERT INTO deploy_runs (id, target_version) VALUES ('live-run', $1)")
                .bind(target)
                .execute(control.pool())
                .await
                .expect("seed live run");
            assert_eq!(
                finalize_abandoned_runs(&control, threshold)
                    .await
                    .expect("finalize with live run"),
                0,
                "a fresh migrating row is a live pod, not debris"
            );
            match advance_deploy(&control).await.expect("advance attempt") {
                AdvanceOutcome::NothingToAdvance { status } => assert_eq!(status, "migrating"),
                other => panic!("expected NothingToAdvance, got {other:?}"),
            }
        },
    )
    .await;
}

// ==================== Bookkeeping retries ====================

/// The gate's bookkeeping writes retry on transient control-plane errors
/// instead of wedging the pod in migrating mode forever (the silent stalled
/// rollout: liveness green, readiness 503, orchestrator never restarts).
/// Honest injection: a trigger refuses `deploy_runs` INSERTs while a
/// sentinel row exists — a real SQL error through the production path — and
/// the fault clears mid-gate.
#[tokio::test]
async fn bookkeeping_retries_survive_a_transient_control_plane_fault() {
    with_control_db(
        "bookkeeping_retries_survive_a_transient_control_plane_fault",
        |url| async move {
            let control = connect_control(&url).await;
            sqlx::raw_sql(
                "CREATE TABLE fault_sentinel (armed INT); \
                 INSERT INTO fault_sentinel VALUES (1); \
                 CREATE FUNCTION reject_deploy_runs() RETURNS trigger AS $$ \
                 BEGIN \
                     IF EXISTS (SELECT 1 FROM fault_sentinel) THEN \
                         RAISE EXCEPTION 'injected: transient control-plane fault'; \
                     END IF; \
                     RETURN NEW; \
                 END $$ LANGUAGE plpgsql; \
                 CREATE TRIGGER reject_deploy_runs_trigger \
                     BEFORE INSERT ON deploy_runs \
                     FOR EACH ROW EXECUTE FUNCTION reject_deploy_runs();",
            )
            .execute(control.pool())
            .await
            .expect("arm control-plane fault");

            let readiness = Readiness::new(control.clone());
            let gate = tokio::spawn(run_fleet_gate(
                control.clone(),
                cluster_config(),
                fast_config(),
                DeployPolicy::default(),
                readiness.clone(),
            ));

            // While the fault holds, the pod is migrating — retrying, not
            // dead.
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert!(
                !readiness.is_ready().await,
                "still migrating under the fault"
            );
            assert!(
                latest_deploy_run(&control)
                    .await
                    .expect("read runs")
                    .is_none(),
                "no run row landed yet"
            );

            // The fault clears; the gate's next 5s retry records the run,
            // the (empty) fleet migrates vacuously, and readiness flips.
            sqlx::query("DELETE FROM fault_sentinel")
                .execute(control.pool())
                .await
                .expect("clear fault");
            gate.await.expect("gate task");
            assert!(
                readiness.is_ready().await,
                "one transient bookkeeping error must not wedge the pod"
            );
            let run = latest_deploy_run(&control)
                .await
                .expect("read latest run")
                .expect("the retried start landed");
            assert_eq!(run.deploy_status, "ready");
            assert!(run.finished_at.is_some());
        },
    )
    .await;
}

// ==================== The HTTP surface ====================

/// `/ready` at the HTTP level: public (no auth, no tenant Host) on the
/// composed cloud app, 503 while the process is in migrating mode, 200 once
/// the gate admits — while `/health` (liveness) is 200 throughout. This is
/// the liveness/readiness split orchestrators key on: a migrating pod stays
/// alive but unrouted.
#[actix_web::test] // actix runtime: the harness spawns the server with rt::spawn.
async fn ready_route_is_public_and_tracks_the_gate() {
    with_control_db(
        "ready_route_is_public_and_tracks_the_gate",
        |url| async move {
            let control = connect_control(&url).await;
            let cluster = cluster_config();
            let cache = Arc::new(AccountCache::new(
                control.clone(),
                cluster.clone(),
                support::test_vault(),
                AccountCacheConfig::default(),
            ));
            let auth = CloudAuth::new(control.clone(), Arc::clone(&cache), BASE_DOMAIN);
            let account_plane = AccountPlane::new(
                control.clone(),
                cluster.clone(),
                ManagedKeys::Disabled,
                Arc::new(CapturingSender::default()),
                AccountPlaneConfig::new(BASE_DOMAIN),
            )
            .expect("build account plane");
            let tenant_plane = TenantPlane::new(
                control.clone(),
                cluster.clone(),
                ManagedKeys::Disabled,
                support::test_vault(),
                Arc::clone(&cache),
            );
            let fallback = FallbackAppState::build().expect("build fallback state");

            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
            let port = listener.local_addr().expect("local addr").port();
            let state = fallback.data();
            let control_for_app = control.clone();
            let chat_streams = ChatStreamLimiter::new(DEFAULT_CHAT_STREAMS_PER_ACCOUNT);
            // Boot state: migrating, exactly as `serve` starts.
            let readiness = Readiness::new(control.clone());
            let readiness_for_app = readiness.clone();
            let server = HttpServer::new(move || {
                App::new().configure(configure_cloud_app(
                    state.clone(),
                    auth.clone(),
                    account_plane.clone(),
                    tenant_plane.clone(),
                    control_for_app.clone(),
                    chat_streams.clone(),
                    readiness_for_app.clone(),
                ))
            })
            .workers(1)
            .listen(listener)
            .expect("attach listener")
            .run();
            let handle = server.handle();
            actix_web::rt::spawn(server);

            let client = reqwest::Client::new();
            let base = format!("http://127.0.0.1:{port}");

            // Migrating mode: liveness up, readiness 503 — no auth, no Host.
            let health = client
                .get(format!("{base}/health"))
                .send()
                .await
                .expect("GET /health");
            assert_eq!(
                health.status().as_u16(),
                200,
                "liveness is up while migrating"
            );
            let ready = client
                .get(format!("{base}/ready"))
                .send()
                .await
                .expect("GET /ready");
            assert_eq!(ready.status().as_u16(), 503);
            let body: serde_json::Value = ready.json().await.expect("ready body");
            assert_eq!(body["status"], "migrating");

            // An empty fleet (nothing lags) is vacuously healthy: the gate
            // completes immediately and flips ready.
            run_fleet_gate(
                control.clone(),
                cluster.clone(),
                fast_config(),
                DeployPolicy::default(),
                readiness.clone(),
            )
            .await;

            let ready = client
                .get(format!("{base}/ready"))
                .send()
                .await
                .expect("GET /ready");
            assert_eq!(
                ready.status().as_u16(),
                200,
                "the gate's flip is visible at /ready"
            );
            let body: serde_json::Value = ready.json().await.expect("ready body");
            assert_eq!(body["status"], "ready");

            handle.stop(false).await;
        },
    )
    .await;
}
