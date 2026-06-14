//! Account provisioning and deletion integration tests.
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command. Tenant databases provisioned here are
//! dropped by the support guard via the control database's references.

mod support;

use atomic_cloud::provision::is_tenant_db_name;
use atomic_cloud::{
    delete_account, provision_account, CloudError, ClusterConfig, ControlPlane, ManagedKeys,
    NewAccount,
};
use atomic_core::DatabaseManager;
use sqlx::{Connection, PgConnection};
use support::{create_database, with_control_db, with_db_guard, with_db_name, TEST_DB_PREFIX};

/// Migrated control plane + a cluster config pointing at the test cluster.
async fn setup(control_url: &str) -> (ControlPlane, ClusterConfig) {
    let control = ControlPlane::connect(control_url)
        .await
        .expect("connect control plane");
    control.initialize().await.expect("migrate control plane");
    let cluster = ClusterConfig {
        cluster_id: "test-cluster-1".to_string(),
        cluster_url: std::env::var("ATOMIC_TEST_DATABASE_URL")
            .expect("with_control_db verified ATOMIC_TEST_DATABASE_URL"),
    };
    (control, cluster)
}

fn new_account(email: &str, subdomain: &str) -> NewAccount {
    NewAccount {
        email: email.to_string(),
        subdomain: subdomain.to_string(),
    }
}

async fn database_exists(base_url: &str, db_name: &str) -> bool {
    let mut conn = PgConnection::connect(base_url)
        .await
        .expect("connect for pg_database check");
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(db_name)
            .fetch_one(&mut conn)
            .await
            .expect("query pg_database");
    let _ = conn.close().await;
    exists
}

async fn account_status(control: &ControlPlane, account_id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(control.pool())
        .await
        .expect("read account status")
}

async fn count(control: &ControlPlane, sql: &str, bind: &str) -> i64 {
    sqlx::query_scalar(sql)
        .bind(bind)
        .fetch_one(control.pool())
        .await
        .expect("count query")
}

#[tokio::test]
async fn provision_happy_path() {
    with_control_db("provision_happy_path", |url| async move {
        let (control, cluster) = setup(&url).await;

        let acct = provision_account(
            &control,
            &cluster,
            &ManagedKeys::Disabled,
            new_account("kenny@example.com", "kenny"),
        )
        .await
        .expect("provision succeeds");

        assert_eq!(acct.subdomain, "kenny");
        assert!(
            is_tenant_db_name(&acct.db_name),
            "tenant db name {} has the generated shape",
            acct.db_name
        );

        // Control plane: account active, mapping row carries the configured
        // cluster id.
        assert_eq!(account_status(&control, &acct.account_id).await, "active");
        let (cluster_id, db_status): (String, String) = sqlx::query_as(
            "SELECT cluster_id, status FROM account_databases \
             WHERE account_id = $1 AND db_name = $2",
        )
        .bind(&acct.account_id)
        .bind(&acct.db_name)
        .fetch_one(control.pool())
        .await
        .expect("account_databases row exists");
        assert_eq!(cluster_id, "test-cluster-1");
        assert_eq!(db_status, "active");

        // Cluster: the tenant database physically exists.
        assert!(database_exists(&cluster.cluster_url, &acct.db_name).await);

        // Tenant: default knowledge base seeded by the production code path.
        let tenant_url = cluster.tenant_db_url(&acct.db_name).expect("tenant url");
        let mut tenant = PgConnection::connect(&tenant_url)
            .await
            .expect("connect tenant db");
        let (kb_id, kb_name, is_default): (String, String, i32) =
            sqlx::query_as("SELECT id, name, is_default FROM databases")
                .fetch_one(&mut tenant)
                .await
                .expect("exactly one seeded databases row");
        assert_eq!(
            (kb_id.as_str(), kb_name.as_str(), is_default),
            ("default", "Default", 1)
        );
        let tenant_version: i32 =
            sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM schema_version")
                .fetch_one(&mut tenant)
                .await
                .expect("read tenant schema_version");
        let _ = tenant.close().await;
        assert!(tenant_version > 0, "tenant migrations ran");

        // The tenant schema must sit at atomic-core's current target. Rather
        // than hardcoding the version, migrate a fresh reference database
        // through the same production path and compare.
        let ref_name = format!("{TEST_DB_PREFIX}ref_{}", uuid::Uuid::new_v4().simple());
        create_database(&cluster.cluster_url, &ref_name).await;
        let ref_url = with_db_name(&cluster.cluster_url, &ref_name);
        with_db_guard(&cluster.cluster_url, &ref_name, || async move {
            let manager = DatabaseManager::new_postgres(".", &ref_url)
                .await
                .expect("migrate reference database");
            drop(manager);
            let mut reference = PgConnection::connect(&ref_url)
                .await
                .expect("connect reference db");
            let ref_version: i32 =
                sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM schema_version")
                    .fetch_one(&mut reference)
                    .await
                    .expect("read reference schema_version");
            let _ = reference.close().await;
            assert_eq!(
                tenant_version, ref_version,
                "tenant schema_version matches atomic-core's current target"
            );
        })
        .await;
    })
    .await;
}

#[tokio::test]
async fn provision_duplicate_subdomain_is_taken() {
    with_control_db("provision_duplicate_subdomain_is_taken", |url| async move {
        let (control, cluster) = setup(&url).await;

        provision_account(
            &control,
            &cluster,
            &ManagedKeys::Disabled,
            new_account("first@example.com", "shared"),
        )
        .await
        .expect("first provision succeeds");

        // A different email racing for the same subdomain.
        let err = provision_account(
            &control,
            &cluster,
            &ManagedKeys::Disabled,
            new_account("second@example.com", "shared"),
        )
        .await
        .expect_err("duplicate subdomain must fail");
        assert!(
            matches!(err, CloudError::SubdomainTaken(ref s) if s == "shared"),
            "expected SubdomainTaken, got {err:?}"
        );

        // Same email, but the account is already active — not a resumable
        // stuck provision, so still taken.
        let err = provision_account(
            &control,
            &cluster,
            &ManagedKeys::Disabled,
            new_account("first@example.com", "shared"),
        )
        .await
        .expect_err("re-signup of an active account must fail");
        assert!(matches!(err, CloudError::SubdomainTaken(_)));

        assert_eq!(
            count(
                &control,
                "SELECT COUNT(*) FROM accounts WHERE subdomain = $1",
                "shared"
            )
            .await,
            1,
            "failed claims must not leave extra rows"
        );
    })
    .await;
}

#[tokio::test]
async fn provision_rejects_reserved_and_invalid_subdomains() {
    with_control_db(
        "provision_rejects_reserved_and_invalid_subdomains",
        |url| async move {
            let (control, cluster) = setup(&url).await;

            // Static blocklist.
            let err = provision_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                new_account("k@example.com", "admin"),
            )
            .await
            .expect_err("blocklisted subdomain must fail");
            assert!(matches!(err, CloudError::SubdomainReserved(ref s) if s == "admin"));

            // Active hold in subdomains_reserved (post-deletion park).
            sqlx::query(
                "INSERT INTO subdomains_reserved (subdomain, expires_at) \
                 VALUES ('parked', NOW() + INTERVAL '1 day')",
            )
            .execute(control.pool())
            .await
            .expect("insert active reservation");
            let err = provision_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                new_account("k@example.com", "parked"),
            )
            .await
            .expect_err("actively reserved subdomain must fail");
            assert!(matches!(err, CloudError::SubdomainReserved(ref s) if s == "parked"));

            // An expired hold no longer blocks — the 90-day park lapses.
            sqlx::query(
                "INSERT INTO subdomains_reserved (subdomain, expires_at) \
                 VALUES ('lapsed', NOW() - INTERVAL '1 day')",
            )
            .execute(control.pool())
            .await
            .expect("insert expired reservation");
            provision_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                new_account("k@example.com", "lapsed"),
            )
            .await
            .expect("expired reservation must not block");

            // Slug-rule violations never reach the database.
            for bad in ["ab", "Has-Upper", "under_score"] {
                let err = provision_account(
                    &control,
                    &cluster,
                    &ManagedKeys::Disabled,
                    new_account("k@example.com", bad),
                )
                .await
                .expect_err("invalid subdomain must fail");
                assert!(
                    matches!(err, CloudError::InvalidSubdomain(_)),
                    "expected InvalidSubdomain for {bad:?}, got {err:?}"
                );
            }

            // Email shape check.
            let err = provision_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                new_account("not-an-email", "fine-name"),
            )
            .await
            .expect_err("invalid email must fail");
            assert!(matches!(err, CloudError::InvalidEmail(_)));

            assert_eq!(
                count(
                    &control,
                    "SELECT COUNT(*) FROM accounts WHERE subdomain <> $1",
                    "lapsed"
                )
                .await,
                0,
                "rejected signups must not leave account rows"
            );
        },
    )
    .await;
}

#[tokio::test]
async fn provision_resumes_after_crash() {
    with_control_db("provision_resumes_after_crash", |url| async move {
        let (control, cluster) = setup(&url).await;
        let signup = new_account("kenny@example.com", "resumable");

        let first = provision_account(&control, &cluster, &ManagedKeys::Disabled, signup.clone())
            .await
            .expect("initial provision");

        // Simulate a crash mid-provision: the account row exists but never
        // reached 'active'. Every real step (database, migrations, seeds,
        // mapping row) is already done — the re-run must converge anyway.
        sqlx::query("UPDATE accounts SET status = 'provisioning' WHERE id = $1")
            .bind(&first.account_id)
            .execute(control.pool())
            .await
            .expect("flip status back to provisioning");

        let second = provision_account(&control, &cluster, &ManagedKeys::Disabled, signup)
            .await
            .expect("re-run completes the stuck provision");

        assert_eq!(
            second.account_id, first.account_id,
            "resume, not a new account"
        );
        assert_eq!(second.db_name, first.db_name);
        assert_eq!(account_status(&control, &first.account_id).await, "active");
        assert_eq!(
            count(
                &control,
                "SELECT COUNT(*) FROM account_databases WHERE account_id = $1",
                &first.account_id,
            )
            .await,
            1,
            "resume must not duplicate account_databases rows"
        );
        assert_eq!(
            count(
                &control,
                "SELECT COUNT(*) FROM accounts WHERE subdomain = $1",
                "resumable"
            )
            .await,
            1
        );
    })
    .await;
}

/// Provisioning stamps the migration-tracking columns (plan: "Schema
/// migration on deploy" — fresh tenants are never stragglers): the mapping
/// row records the compiled tenant schema target the provision just
/// migrated the tenant to, and a resumed provision re-records its success
/// without ever regressing a higher stamp.
#[tokio::test]
async fn provision_stamps_migration_tracking() {
    with_control_db("provision_stamps_migration_tracking", |url| async move {
        let (control, cluster) = setup(&url).await;
        let target = atomic_cloud::tenant_schema_target();

        let acct = provision_account(
            &control,
            &cluster,
            &ManagedKeys::Disabled,
            new_account("kenny@example.com", "stamped"),
        )
        .await
        .expect("provision succeeds");

        type Tracking = (
            i32,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<String>,
            i32,
        );
        let read_tracking = || async {
            let row: Tracking = sqlx::query_as(
                "SELECT last_migrated_version, last_migrated_at, migration_failed_at, \
                        last_migration_error, migration_retry_count \
                 FROM account_databases WHERE account_id = $1",
            )
            .bind(&acct.account_id)
            .fetch_one(control.pool())
            .await
            .expect("read tracking columns");
            row
        };

        let (version, at, failed_at, error, retries) = read_tracking().await;
        assert_eq!(
            version, target,
            "fresh provision stamps the compiled target"
        );
        assert!(at.is_some(), "fresh provision records last_migrated_at");
        assert!(failed_at.is_none());
        assert!(error.is_none());
        assert_eq!(retries, 0);

        // A resumed provision re-runs the tenant migrations and re-records
        // the success — including clearing failure state a prior attempt
        // left behind. Drive the exact pre-resume state by SQL.
        sqlx::query("UPDATE accounts SET status = 'provisioning' WHERE id = $1")
            .bind(&acct.account_id)
            .execute(control.pool())
            .await
            .expect("flip status back to provisioning");
        sqlx::query(
            "UPDATE account_databases \
             SET last_migrated_version = 0, last_migrated_at = NULL, \
                 migration_failed_at = NOW(), last_migration_error = 'boom', \
                 migration_retry_after = NOW() + INTERVAL '1 hour', \
                 migration_retry_count = 3 \
             WHERE account_id = $1",
        )
        .bind(&acct.account_id)
        .execute(control.pool())
        .await
        .expect("simulate a failed pre-resume state");

        provision_account(
            &control,
            &cluster,
            &ManagedKeys::Disabled,
            new_account("kenny@example.com", "stamped"),
        )
        .await
        .expect("resume completes");

        let (version, at, failed_at, error, retries) = read_tracking().await;
        assert_eq!(version, target, "resume re-stamps the compiled target");
        assert!(at.is_some());
        assert!(failed_at.is_none(), "resume clears failure state");
        assert!(error.is_none());
        assert_eq!(retries, 0);

        // GREATEST: a stamp from a newer binary survives an older binary's
        // resume (rolling deploys must not regress the recorded version).
        sqlx::query("UPDATE accounts SET status = 'provisioning' WHERE id = $1")
            .bind(&acct.account_id)
            .execute(control.pool())
            .await
            .expect("flip status back to provisioning again");
        sqlx::query(
            "UPDATE account_databases SET last_migrated_version = $2 WHERE account_id = $1",
        )
        .bind(&acct.account_id)
        .bind(target + 5)
        .execute(control.pool())
        .await
        .expect("simulate a newer binary's stamp");

        provision_account(
            &control,
            &cluster,
            &ManagedKeys::Disabled,
            new_account("kenny@example.com", "stamped"),
        )
        .await
        .expect("resume completes again");

        let (version, ..) = read_tracking().await;
        assert_eq!(version, target + 5, "resume never regresses a higher stamp");
    })
    .await;
}

/// Issue: the `subdomains_reserved` pre-check and the claim INSERT are not
/// atomic, so a concurrent `delete_account` can park the subdomain between
/// them. The post-claim re-check (`ensure_claim_not_reserved`, the seam
/// `provision_account` runs right after a fresh claim) must roll the claim
/// back. The test drives the exact interleaved state by direct SQL: claim
/// row inserted (pre-check already passed), then the deletion's hold lands.
#[tokio::test]
async fn claim_is_rolled_back_when_reservation_lands_mid_provision() {
    with_control_db(
        "claim_is_rolled_back_when_reservation_lands_mid_provision",
        |url| async move {
            let (control, _cluster) = setup(&url).await;

            // The provision passed its pre-check (no hold yet) and claimed
            // the subdomain...
            let account_id = uuid::Uuid::new_v4();
            sqlx::query(
                "INSERT INTO accounts (id, subdomain, email, status, plan) \
                 VALUES ($1, 'contested', 'k@example.com', 'provisioning', 'free')",
            )
            .bind(account_id.to_string())
            .execute(control.pool())
            .await
            .expect("insert claim row");

            // ...and a concurrent delete_account parked the same subdomain.
            // (Deletion writes this hold BEFORE hard-deleting its accounts
            // row — the ordering that makes the post-claim re-check
            // sufficient.)
            sqlx::query(
                "INSERT INTO subdomains_reserved (subdomain, expires_at) \
                 VALUES ('contested', NOW() + INTERVAL '90 days')",
            )
            .execute(control.pool())
            .await
            .expect("park subdomain");

            let err = atomic_cloud::provision::ensure_claim_not_reserved(
                &control,
                account_id,
                "contested",
            )
            .await
            .expect_err("post-claim re-check must reject the parked subdomain");
            assert!(
                matches!(err, CloudError::SubdomainReserved(ref s) if s == "contested"),
                "expected SubdomainReserved, got {err:?}"
            );
            assert_eq!(
                count(
                    &control,
                    "SELECT COUNT(*) FROM accounts WHERE subdomain = $1",
                    "contested"
                )
                .await,
                0,
                "the losing claim must be rolled back"
            );

            // Without a hold the guard passes and leaves the claim alone.
            let unchallenged = uuid::Uuid::new_v4();
            sqlx::query(
                "INSERT INTO accounts (id, subdomain, email, status, plan) \
                 VALUES ($1, 'unchallenged', 'k@example.com', 'provisioning', 'free')",
            )
            .bind(unchallenged.to_string())
            .execute(control.pool())
            .await
            .expect("insert claim row");
            atomic_cloud::provision::ensure_claim_not_reserved(
                &control,
                unchallenged,
                "unchallenged",
            )
            .await
            .expect("no hold, no rollback");
            assert_eq!(
                count(
                    &control,
                    "SELECT COUNT(*) FROM accounts WHERE subdomain = $1",
                    "unchallenged"
                )
                .await,
                1,
                "an unchallenged claim must survive the re-check"
            );
        },
    )
    .await;
}

/// Issue: a `delete_account` completing while a resumed provision is in
/// flight (claim done, migrations running) removes the accounts row out
/// from under it; the provision's `account_databases` INSERT then hits a
/// foreign-key violation — and without cleanup the tenant database it just
/// re-created would be orphaned forever (no accounts row left to derive its
/// name from). Drives the real interleaving: a resumed provision runs in a
/// task, and the accounts row is deleted by direct SQL the moment the
/// tenant database appears in `pg_database` (migrations still have hundreds
/// of milliseconds to run, so the row is gone before the INSERT).
#[tokio::test]
async fn racing_deletion_does_not_orphan_tenant_database() {
    with_control_db(
        "racing_deletion_does_not_orphan_tenant_database",
        |url| async move {
            let (control, cluster) = setup(&url).await;

            // Seed a crashed provision with a *known* account id so the
            // tenant database name is known up front; provision_account
            // resumes it (same email + subdomain, status 'provisioning').
            let account_id = uuid::Uuid::new_v4();
            sqlx::query(
                "INSERT INTO accounts (id, subdomain, email, status, plan) \
                 VALUES ($1, 'race-victim', 'r@example.com', 'provisioning', 'free')",
            )
            .bind(account_id.to_string())
            .execute(control.pool())
            .await
            .expect("seed crashed provision");
            let db_name = atomic_cloud::tenant_db_name(account_id);

            // Extra guard beyond with_control_db's bookkeeping: if the test
            // fails *because* the database was orphaned, the accounts row is
            // gone and the support cleanup can no longer discover the name.
            with_db_guard(&cluster.cluster_url, &db_name, || async {
                // Run the provision and the saboteur concurrently in one
                // task (join!, not spawn — provision_account's sqlx futures
                // trip rustc's "implementation is not general enough"
                // higher-ranked lifetime check under spawn's Send bound).
                let provision = provision_account(
                    &control,
                    &cluster,
                    &ManagedKeys::Disabled,
                    new_account("r@example.com", "race-victim"),
                );
                let saboteur = async {
                    // Wait for CREATE DATABASE to land...
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                    while !database_exists(&cluster.cluster_url, &db_name).await {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "tenant database never appeared; provision stalled"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                    // ...then yank the accounts row, as the tail end of a
                    // concurrent delete_account would.
                    sqlx::query("DELETE FROM accounts WHERE id = $1")
                        .bind(account_id.to_string())
                        .execute(control.pool())
                        .await
                        .expect("delete accounts row mid-provision");
                };

                let (provision_result, ()) = tokio::join!(provision, saboteur);
                let err =
                    provision_result.expect_err("provision must fail once its account is gone");
                assert!(
                    matches!(err, CloudError::AccountNoLongerProvisioning(ref id)
                        if *id == account_id.to_string()),
                    "expected AccountNoLongerProvisioning, got {err:?}"
                );
                assert!(
                    !database_exists(&cluster.cluster_url, &db_name).await,
                    "the losing provision must drop the tenant database it created"
                );
                assert_eq!(
                    count(
                        &control,
                        "SELECT COUNT(*) FROM account_databases WHERE account_id = $1",
                        &account_id.to_string(),
                    )
                    .await,
                    0,
                    "no mapping row may survive"
                );
            })
            .await;
        },
    )
    .await;
}

/// Issue: step 11 (activation) must verify its UPDATE matched a row — a
/// concurrent deletion/rollback can remove the accounts row between the
/// mapping insert and activation, and returning `Ok` there would report
/// success for a dead account (session insert hits the FK, the reaper logs
/// a resume that didn't happen, the CLI prints success). That window is two
/// pool round-trips wide, so this drives the activation seam directly (the
/// `ensure_claim_not_reserved` test pattern): a live claim activates and
/// re-activates idempotently; a vanished row is the typed error, never Ok.
#[tokio::test]
async fn activation_of_vanished_account_is_typed_error() {
    with_control_db(
        "activation_of_vanished_account_is_typed_error",
        |url| async move {
            let (control, _cluster) = setup(&url).await;

            let account_id = uuid::Uuid::new_v4();
            sqlx::query(
                "INSERT INTO accounts (id, subdomain, email, status, plan) \
                 VALUES ($1, 'activatable', 'k@example.com', 'provisioning', 'free')",
            )
            .bind(account_id.to_string())
            .execute(control.pool())
            .await
            .expect("seed claim row");

            // A live claim activates...
            atomic_cloud::provision::activate_account(&control, account_id)
                .await
                .expect("live claim activates");
            assert_eq!(
                account_status(&control, &account_id.to_string()).await,
                "active"
            );
            // ...idempotently (resumed provisions re-run step 11).
            atomic_cloud::provision::activate_account(&control, account_id)
                .await
                .expect("re-activation is idempotent");

            // The row vanishes (a concurrent delete_account or reaper
            // rollback won): activation must fail typed, never Ok.
            sqlx::query("DELETE FROM accounts WHERE id = $1")
                .bind(account_id.to_string())
                .execute(control.pool())
                .await
                .expect("yank accounts row");
            let err = atomic_cloud::provision::activate_account(&control, account_id)
                .await
                .expect_err("activating a vanished account must fail");
            assert!(
                matches!(err, CloudError::AccountNoLongerProvisioning(ref id)
                    if *id == account_id.to_string()),
                "expected AccountNoLongerProvisioning, got {err:?}"
            );
        },
    )
    .await;
}

#[tokio::test]
async fn delete_account_removes_everything_and_parks_subdomain() {
    with_control_db(
        "delete_account_removes_everything_and_parks_subdomain",
        |url| async move {
            let (control, cluster) = setup(&url).await;

            let acct = provision_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                new_account("doomed@example.com", "doomed"),
            )
            .await
            .expect("provision");

            // Live credentials that deletion must sweep away.
            atomic_cloud::issue_token(
                &control,
                &acct.account_id,
                atomic_cloud::TokenScope::Account,
                None,
                "laptop",
            )
            .await
            .expect("issue token");
            atomic_cloud::create_session(
                &control,
                &acct.account_id,
                std::time::Duration::from_secs(3600),
                None,
                None,
            )
            .await
            .expect("create session");

            delete_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                None,
                &acct.account_id,
            )
            .await
            .expect("delete succeeds");

            assert!(
                !database_exists(&cluster.cluster_url, &acct.db_name).await,
                "tenant database must be dropped"
            );
            for (table, sql) in [
                ("accounts", "SELECT COUNT(*) FROM accounts WHERE id = $1"),
                (
                    "account_databases",
                    "SELECT COUNT(*) FROM account_databases WHERE account_id = $1",
                ),
                (
                    "cloud_tokens",
                    "SELECT COUNT(*) FROM cloud_tokens WHERE account_id = $1",
                ),
                (
                    "sessions",
                    "SELECT COUNT(*) FROM sessions WHERE account_id = $1",
                ),
            ] {
                assert_eq!(
                    count(&control, sql, &acct.account_id).await,
                    0,
                    "{table} rows must be gone"
                );
            }

            // The freed subdomain is parked for ~90 days.
            let still_held: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM subdomains_reserved \
                 WHERE subdomain = $1 AND expires_at > NOW() + INTERVAL '89 days')",
            )
            .bind(&acct.subdomain)
            .fetch_one(control.pool())
            .await
            .expect("query reservation");
            assert!(still_held, "subdomain must be reserved for 90 days");

            // And a fresh signup for it is rejected while parked.
            let err = provision_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                new_account("new@example.com", "doomed"),
            )
            .await
            .expect_err("parked subdomain must reject signups");
            assert!(matches!(err, CloudError::SubdomainReserved(_)));

            // Re-delete is a no-op, as is deleting an unknown account.
            delete_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                None,
                &acct.account_id,
            )
            .await
            .expect("re-delete is a no-op");
            delete_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                None,
                &uuid::Uuid::new_v4().to_string(),
            )
            .await
            .expect("deleting an unknown account is a no-op");
        },
    )
    .await;
}
