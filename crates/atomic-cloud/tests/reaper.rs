//! Reaper integration tests (plan: "Failure recovery & the reaper").
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command. Tests manufacture the exact crash
//! states the reaper exists for — stale `'provisioning'` rows with
//! half-created tenant databases, orphaned `acct_*` databases,
//! self-reservations — then drive [`run_reaper_pass`] directly and assert
//! on both the database state and the returned [`ReaperSummary`] (which is
//! how advisory-lock skips are observable).

mod support;

use std::time::Duration;

use atomic_cloud::reaper::{reaper_lock_key, run_reaper_pass, ReaperPolicy, ReaperSummary};
use atomic_cloud::{
    provision_account, tenant_db_name, ClusterConfig, ControlPlane, NewAccount, ProvisionedAccount,
};
use sqlx::{Connection, PgConnection};
use support::{create_database, drop_database, with_control_db, with_db_guard};
use uuid::Uuid;

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

/// Insert an accounts row in `status='provisioning'`, backdated by
/// `minutes_old` — the shape a crashed signup leaves behind.
async fn seed_provisioning_row(
    control: &ControlPlane,
    account_id: Uuid,
    email: &str,
    subdomain: &str,
    minutes_old: i32,
) {
    sqlx::query(
        "INSERT INTO accounts (id, subdomain, email, status, plan, created_at) \
         VALUES ($1, $2, $3, 'provisioning', 'free', NOW() - make_interval(mins => $4))",
    )
    .bind(account_id.to_string())
    .bind(subdomain)
    .bind(email)
    .bind(minutes_old)
    .execute(control.pool())
    .await
    .expect("seed provisioning row");
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

async fn account_status(control: &ControlPlane, account_id: &str) -> Option<String> {
    sqlx::query_scalar("SELECT status FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_optional(control.pool())
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

fn assert_no_errors(summary: &ReaperSummary) {
    assert!(
        summary.errors.is_empty(),
        "pass must not record errors: {:?}",
        summary.errors
    );
}

/// A stale provision whose remaining steps CAN complete must be resumed,
/// not rolled back: `CREATE DATABASE` landed but the original signup died
/// before migrations/seeding/mapping/activation. After the pass the account
/// is active with a fully migrated tenant.
#[tokio::test]
async fn stale_half_provision_is_resumed_to_active() {
    with_control_db(
        "stale_half_provision_is_resumed_to_active",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            let account_id = Uuid::new_v4();
            seed_provisioning_row(&control, account_id, "kenny@example.com", "revivable", 10).await;
            let db_name = tenant_db_name(account_id);
            // The crash point: database created, nothing else done.
            create_database(&cluster.cluster_url, &db_name).await;

            with_db_guard(&cluster.cluster_url, &db_name, || async {
                let summary = run_reaper_pass(&control, &cluster, &ReaperPolicy::default()).await;
                assert_no_errors(&summary);
                assert_eq!(summary.stuck_resumed, vec![account_id.to_string()]);
                assert!(summary.stuck_rolled_back.is_empty());

                assert_eq!(
                    account_status(&control, &account_id.to_string())
                        .await
                        .as_deref(),
                    Some("active")
                );
                assert_eq!(
                    count(
                        &control,
                        "SELECT COUNT(*) FROM account_databases WHERE account_id = $1",
                        &account_id.to_string(),
                    )
                    .await,
                    1,
                    "resume records exactly one mapping row"
                );

                // "And serves": the tenant was migrated and seeded through the
                // production path — the default knowledge base exists.
                let tenant_url = cluster.tenant_db_url(&db_name).expect("tenant url");
                let mut tenant = PgConnection::connect(&tenant_url)
                    .await
                    .expect("connect resumed tenant db");
                let kb_id: String =
                    sqlx::query_scalar("SELECT id FROM databases WHERE is_default = 1")
                        .fetch_one(&mut tenant)
                        .await
                        .expect("default KB seeded");
                let _ = tenant.close().await;
                assert_eq!(kb_id, "default");
            })
            .await;
        },
    )
    .await;
}

/// A stale provision whose resume CANNOT complete (here: a corrupt email
/// that fails provisioning's validation) must be rolled back: tenant
/// database dropped, control-plane rows hard-deleted, and the subdomain
/// immediately claimable by a fresh signup.
#[tokio::test]
async fn unresumable_stale_provision_is_rolled_back() {
    with_control_db(
        "unresumable_stale_provision_is_rolled_back",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            let account_id = Uuid::new_v4();
            // "not-an-email" fails provision_account's email validation, so the
            // resume attempt deterministically fails before touching anything.
            seed_provisioning_row(&control, account_id, "not-an-email", "scorched", 10).await;
            let db_name = tenant_db_name(account_id);
            create_database(&cluster.cluster_url, &db_name).await;
            // This crash got further: the mapping row was written too.
            sqlx::query(
                "INSERT INTO account_databases (account_id, cluster_id, db_name, status) \
             VALUES ($1, 'test-cluster-1', $2, 'active')",
            )
            .bind(account_id.to_string())
            .bind(&db_name)
            .execute(control.pool())
            .await
            .expect("seed mapping row");

            with_db_guard(&cluster.cluster_url, &db_name, || async {
                let summary = run_reaper_pass(&control, &cluster, &ReaperPolicy::default()).await;
                assert_no_errors(&summary);
                assert_eq!(summary.stuck_rolled_back, vec![account_id.to_string()]);
                assert!(summary.stuck_resumed.is_empty());

                assert!(
                    !database_exists(&cluster.cluster_url, &db_name).await,
                    "tenant database must be dropped"
                );
                assert_eq!(
                    account_status(&control, &account_id.to_string()).await,
                    None,
                    "accounts row must be hard-deleted (no 'failed' tombstone)"
                );
                assert_eq!(
                    count(
                        &control,
                        "SELECT COUNT(*) FROM account_databases WHERE account_id = $1",
                        &account_id.to_string(),
                    )
                    .await,
                    0,
                    "mapping row must be gone"
                );

                // The freed subdomain is immediately claimable — no reservation
                // is written for a provision that never activated.
                provision_account(
                    &control,
                    &cluster,
                    new_account("new@example.com", "scorched"),
                )
                .await
                .expect("freed subdomain must be claimable right away");
            })
            .await;
        },
    )
    .await;
}

/// Orphan reclaim: an `acct_*` database with no control-plane rows is
/// dropped; a fully provisioned (referenced) one is untouched.
#[tokio::test]
async fn orphaned_tenant_database_is_reclaimed_and_owned_one_is_not() {
    with_control_db(
        "orphaned_tenant_database_is_reclaimed_and_owned_one_is_not",
        |url| async move {
            let (control, cluster) = setup(&url).await;

            // The non-orphan: a healthy active account.
            let healthy = provision_account(
                &control,
                &cluster,
                new_account("healthy@example.com", "healthy"),
            )
            .await
            .expect("provision healthy account");

            // The orphan: tenant-shaped database, zero control-plane rows —
            // the debris of a failed 23503 cleanup.
            let orphan_db = tenant_db_name(Uuid::new_v4());
            create_database(&cluster.cluster_url, &orphan_db).await;

            with_db_guard(&cluster.cluster_url, &orphan_db, || async {
                let summary = run_reaper_pass(&control, &cluster, &ReaperPolicy::default()).await;
                assert_no_errors(&summary);
                assert_eq!(summary.orphan_dbs_dropped, vec![orphan_db.clone()]);
                assert!(summary.stuck_rolled_back.is_empty());

                assert!(
                    !database_exists(&cluster.cluster_url, &orphan_db).await,
                    "orphan must be dropped"
                );
                assert!(
                    database_exists(&cluster.cluster_url, &healthy.db_name).await,
                    "owned database must be untouched"
                );
                assert_eq!(
                    account_status(&control, &healthy.account_id)
                        .await
                        .as_deref(),
                    Some("active"),
                    "healthy account must be untouched"
                );
            })
            .await;
        },
    )
    .await;
}

/// A fresh `'provisioning'` row — an in-flight signup that just ran
/// `CREATE DATABASE` — must be left strictly alone by every arm: too young
/// for the stuck arm, and its accounts row disqualifies the orphan arm.
#[tokio::test]
async fn in_flight_healthy_provision_is_left_alone() {
    with_control_db(
        "in_flight_healthy_provision_is_left_alone",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            let account_id = Uuid::new_v4();
            seed_provisioning_row(&control, account_id, "live@example.com", "in-flight", 0).await;
            let db_name = tenant_db_name(account_id);
            create_database(&cluster.cluster_url, &db_name).await;

            with_db_guard(&cluster.cluster_url, &db_name, || async {
                let summary = run_reaper_pass(&control, &cluster, &ReaperPolicy::default()).await;
                assert_no_errors(&summary);
                assert!(summary.stuck_resumed.is_empty());
                assert!(summary.stuck_rolled_back.is_empty());
                assert!(summary.stuck_deferred.is_empty());
                assert!(summary.orphan_dbs_dropped.is_empty());

                assert_eq!(
                    account_status(&control, &account_id.to_string())
                        .await
                        .as_deref(),
                    Some("provisioning"),
                    "in-flight row must be untouched"
                );
                assert!(
                    database_exists(&cluster.cluster_url, &db_name).await,
                    "in-flight tenant database must be untouched"
                );
            })
            .await;
        },
    )
    .await;
}

/// A stuck provision whose accounts row is yanked while the reaper's resume
/// is mid-flight (the tail end of a racing `delete_account`) must NOT be
/// reported resumed: the resume aborts typed
/// (`AccountNoLongerProvisioning`), the rollback's status-guarded DELETE
/// finds the row already gone, and the outcome is "already settled" —
/// neither `stuck_resumed` nor `stuck_rolled_back` — with the tenant
/// database the resume created dropped rather than orphaned.
#[tokio::test]
async fn resume_racing_deletion_is_not_reported_resumed() {
    with_control_db(
        "resume_racing_deletion_is_not_reported_resumed",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            let account_id = Uuid::new_v4();
            seed_provisioning_row(&control, account_id, "r@example.com", "yanked", 10).await;
            let db_name = tenant_db_name(account_id);

            with_db_guard(&cluster.cluster_url, &db_name, || async {
                let policy = ReaperPolicy::default();
                let pass = run_reaper_pass(&control, &cluster, &policy);
                let saboteur = async {
                    // Wait for the resume's CREATE DATABASE to land
                    // (migrations still have hundreds of milliseconds to
                    // run), then yank the accounts row, as the tail end of
                    // a concurrent delete_account would.
                    let deadline = std::time::Instant::now() + Duration::from_secs(30);
                    while !database_exists(&cluster.cluster_url, &db_name).await {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "tenant database never appeared; resume stalled"
                        );
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    sqlx::query("DELETE FROM accounts WHERE id = $1")
                        .bind(account_id.to_string())
                        .execute(control.pool())
                        .await
                        .expect("delete accounts row mid-resume");
                };
                let (summary, ()) = tokio::join!(pass, saboteur);

                assert_no_errors(&summary);
                assert!(
                    summary.stuck_resumed.is_empty(),
                    "a dead account must not be reported resumed: {summary:?}"
                );
                assert!(
                    summary.stuck_rolled_back.is_empty(),
                    "nothing was left to roll back: {summary:?}"
                );
                assert_eq!(
                    account_status(&control, &account_id.to_string()).await,
                    None,
                    "the deletion's outcome stands"
                );
                assert!(
                    !database_exists(&cluster.cluster_url, &db_name).await,
                    "the aborted resume must drop the database it created"
                );
            })
            .await;
        },
    )
    .await;
}

/// The interrupted-deletion arm. Three active accounts:
///
/// - one half-deleted past the grace (the REAL deletion steps 1–5 run —
///   tokens revoked, sessions gone, database dropped, mapping row removed —
///   with the accounts row left `'active'`, exactly what a killed
///   `DELETE /api/account` pod leaves) → completed: row gone, subdomain
///   parked;
/// - one healthy, equally old → untouched, pinning the no-false-positive
///   invariant (a healthy active account always has its mapping row);
/// - one half-deleted but inside the grace → deferred this pass.
#[tokio::test]
async fn interrupted_deletion_is_completed_and_healthy_account_untouched() {
    with_control_db(
        "interrupted_deletion_is_completed_and_healthy_account_untouched",
        |url| async move {
            let (control, cluster) = setup(&url).await;

            let healthy = provision_account(
                &control,
                &cluster,
                new_account("keep@example.com", "keeper"),
            )
            .await
            .expect("provision healthy");
            let doomed =
                provision_account(&control, &cluster, new_account("gone@example.com", "goner"))
                    .await
                    .expect("provision doomed");
            let fresh = provision_account(
                &control,
                &cluster,
                new_account("young@example.com", "youngling"),
            )
            .await
            .expect("provision fresh");

            // Age healthy and doomed past the 5-minute grace; fresh stays
            // at NOW().
            for id in [&healthy.account_id, &doomed.account_id] {
                sqlx::query(
                    "UPDATE accounts SET created_at = NOW() - INTERVAL '10 minutes' \
                     WHERE id = $1",
                )
                .bind(id)
                .execute(control.pool())
                .await
                .expect("backdate account");
            }

            run_deletion_steps_through_mapping_removal(&control, &cluster, &doomed).await;
            run_deletion_steps_through_mapping_removal(&control, &cluster, &fresh).await;

            let summary = run_reaper_pass(&control, &cluster, &ReaperPolicy::default()).await;
            assert_no_errors(&summary);
            assert_eq!(
                summary.deletions_completed,
                vec![doomed.account_id.clone()],
                "exactly the aged half-deletion is completed: {summary:?}"
            );
            assert!(summary.deletions_skipped_locked.is_empty());

            // Doomed: deletion completed — row gone, subdomain parked.
            assert_eq!(account_status(&control, &doomed.account_id).await, None);
            let parked: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM subdomains_reserved \
                 WHERE subdomain = 'goner' AND expires_at > NOW())",
            )
            .fetch_one(control.pool())
            .await
            .expect("query reservation");
            assert!(parked, "the completed deletion must park the subdomain");

            // Healthy: untouched in every respect.
            assert_eq!(
                account_status(&control, &healthy.account_id)
                    .await
                    .as_deref(),
                Some("active")
            );
            assert!(database_exists(&cluster.cluster_url, &healthy.db_name).await);
            assert_eq!(
                count(
                    &control,
                    "SELECT COUNT(*) FROM account_databases WHERE account_id = $1",
                    &healthy.account_id,
                )
                .await,
                1
            );

            // Fresh: inside the grace, deferred to a later pass.
            assert_eq!(
                account_status(&control, &fresh.account_id).await.as_deref(),
                Some("active"),
                "a half-deletion inside the grace must be left alone this pass"
            );
        },
    )
    .await;
}

/// The real `delete_account` steps 1–5 — everything before the accounts-row
/// delete — by direct SQL + DROP, manufacturing the exact state a deletion
/// interrupted after the mapping removal leaves behind.
async fn run_deletion_steps_through_mapping_removal(
    control: &ControlPlane,
    cluster: &ClusterConfig,
    acct: &ProvisionedAccount,
) {
    // Step 1 — revoke tokens (idempotent over none).
    sqlx::query("UPDATE cloud_tokens SET revoked_at = NOW() WHERE account_id = $1")
        .bind(&acct.account_id)
        .execute(control.pool())
        .await
        .expect("revoke tokens");
    // Step 2 — delete sessions.
    sqlx::query("DELETE FROM sessions WHERE account_id = $1")
        .bind(&acct.account_id)
        .execute(control.pool())
        .await
        .expect("delete sessions");
    // Step 4 — drop the tenant database.
    drop_database(&cluster.cluster_url, &acct.db_name).await;
    // Step 5 — remove the mapping rows. The crash point: the accounts row
    // is never touched.
    sqlx::query("DELETE FROM account_databases WHERE account_id = $1")
        .bind(&acct.account_id)
        .execute(control.pool())
        .await
        .expect("delete mapping rows");
}

/// Self-reservation cleanup: a reservation parked on an ACTIVE account's
/// own subdomain (crashed-deletion residue) is cleared once past the settle
/// grace; a fresh one (deletion in flight) and an unrelated reservation are
/// both kept.
#[tokio::test]
async fn self_reservation_residue_is_cleared_with_grace() {
    with_control_db(
        "self_reservation_residue_is_cleared_with_grace",
        |url| async move {
            let (control, cluster) = setup(&url).await;

            let _alpha =
                provision_account(&control, &cluster, new_account("a@example.com", "alpha"))
                    .await
                    .expect("provision alpha");
            let _bravo =
                provision_account(&control, &cluster, new_account("b@example.com", "bravo"))
                    .await
                    .expect("provision bravo");

            // Residue: alpha's deletion crashed after reserving, 10 minutes ago.
            sqlx::query(
                "INSERT INTO subdomains_reserved (subdomain, expires_at, created_at) \
             VALUES ('alpha', NOW() + INTERVAL '90 days', NOW() - INTERVAL '10 minutes')",
            )
            .execute(control.pool())
            .await
            .expect("seed residue reservation");
            // In flight: bravo's deletion reserved just now (between its reserve
            // and row-delete steps).
            sqlx::query(
                "INSERT INTO subdomains_reserved (subdomain, expires_at, created_at) \
             VALUES ('bravo', NOW() + INTERVAL '90 days', NOW())",
            )
            .execute(control.pool())
            .await
            .expect("seed fresh reservation");
            // Unrelated: an ordinary post-deletion park with no account.
            sqlx::query(
                "INSERT INTO subdomains_reserved (subdomain, expires_at, created_at) \
             VALUES ('stranger', NOW() + INTERVAL '90 days', NOW() - INTERVAL '10 minutes')",
            )
            .execute(control.pool())
            .await
            .expect("seed unrelated reservation");

            let summary = run_reaper_pass(&control, &cluster, &ReaperPolicy::default()).await;
            assert_no_errors(&summary);
            assert_eq!(summary.self_reservations_cleared, vec!["alpha".to_string()]);

            for (subdomain, expect_held) in [("alpha", false), ("bravo", true), ("stranger", true)]
            {
                assert_eq!(
                    count(
                        &control,
                        "SELECT COUNT(*) FROM subdomains_reserved WHERE subdomain = $1",
                        subdomain,
                    )
                    .await,
                    i64::from(expect_held),
                    "reservation for {subdomain:?}"
                );
            }
        },
    )
    .await;
}

/// Hygiene: long-expired magic links, expired sessions, and lapsed
/// reservations are purged; recently expired links and everything live
/// survive.
#[tokio::test]
async fn hygiene_purges_expired_rows_and_keeps_the_rest() {
    with_control_db(
        "hygiene_purges_expired_rows_and_keeps_the_rest",
        |url| async move {
            let (control, cluster) = setup(&url).await;

            for (hash, offset) in [
                ("link-long-expired", "NOW() - INTERVAL '25 hours'"),
                ("link-just-expired", "NOW() - INTERVAL '1 hour'"),
                ("link-live", "NOW() + INTERVAL '10 minutes'"),
            ] {
                sqlx::query(&format!(
                    "INSERT INTO magic_links (token_hash, email, purpose, expires_at) \
                 VALUES ($1, 'k@example.com', 'login', {offset})"
                ))
                .bind(hash)
                .execute(control.pool())
                .await
                .expect("seed magic link");
            }

            // Sessions need an accounts row for their FK; a bare active row is
            // enough (no tenant database — the orphan arm checks references in
            // the other direction, so this is inert for it).
            let account_id = Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO accounts (id, subdomain, email, status, plan) \
             VALUES ($1, 'hygienic', 'h@example.com', 'active', 'free')",
            )
            .bind(&account_id)
            .execute(control.pool())
            .await
            .expect("seed account");
            for (hash, offset) in [
                ("session-expired", "NOW() - INTERVAL '1 hour'"),
                ("session-live", "NOW() + INTERVAL '1 day'"),
            ] {
                sqlx::query(&format!(
                    "INSERT INTO sessions (hash, account_id, expires_at) VALUES ($1, $2, {offset})"
                ))
                .bind(hash)
                .bind(&account_id)
                .execute(control.pool())
                .await
                .expect("seed session");
            }

            for (subdomain, offset) in [
                ("lapsed-park", "NOW() - INTERVAL '1 hour'"),
                ("held-park", "NOW() + INTERVAL '30 days'"),
            ] {
                sqlx::query(&format!(
                    "INSERT INTO subdomains_reserved (subdomain, expires_at) VALUES ($1, {offset})"
                ))
                .bind(subdomain)
                .execute(control.pool())
                .await
                .expect("seed reservation");
            }

            let summary = run_reaper_pass(&control, &cluster, &ReaperPolicy::default()).await;
            assert_no_errors(&summary);
            assert_eq!(summary.expired_magic_links_purged, 1);
            assert_eq!(summary.expired_sessions_purged, 1);
            assert_eq!(summary.expired_reservations_purged, 1);

            for (sql, survivor) in [
                (
                    "SELECT COUNT(*) FROM magic_links WHERE token_hash = $1",
                    "link-just-expired",
                ),
                (
                    "SELECT COUNT(*) FROM magic_links WHERE token_hash = $1",
                    "link-live",
                ),
                (
                    "SELECT COUNT(*) FROM sessions WHERE hash = $1",
                    "session-live",
                ),
                (
                    "SELECT COUNT(*) FROM subdomains_reserved WHERE subdomain = $1",
                    "held-park",
                ),
            ] {
                assert_eq!(
                    count(&control, sql, survivor).await,
                    1,
                    "{survivor} survives"
                );
            }
            assert_eq!(
                count(
                    &control,
                    "SELECT COUNT(*) FROM magic_links WHERE token_hash = $1",
                    "link-long-expired",
                )
                .await,
                0
            );
        },
    )
    .await;
}

/// The advisory-lock skip, deterministically: while a rival session (a
/// concurrent pass, as far as Postgres is concerned) holds the account's
/// lock, the pass skips the row — observable in the summary — and touches
/// nothing. Once the rival releases, the next pass processes it normally.
#[tokio::test]
async fn contended_lock_skips_row_and_next_pass_recovers() {
    with_control_db(
        "contended_lock_skips_row_and_next_pass_recovers",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            let account_id = Uuid::new_v4();
            seed_provisioning_row(&control, account_id, "kenny@example.com", "contended", 10).await;

            let mut rival = PgConnection::connect(&url).await.expect("rival session");
            let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                .bind(reaper_lock_key(&account_id.to_string()))
                .fetch_one(&mut rival)
                .await
                .expect("rival takes lock");
            assert!(got, "rival lock must be free initially");

            let summary = run_reaper_pass(&control, &cluster, &ReaperPolicy::default()).await;
            assert_no_errors(&summary);
            assert_eq!(summary.stuck_skipped_locked, vec![account_id.to_string()]);
            assert!(summary.stuck_resumed.is_empty());
            assert!(summary.stuck_rolled_back.is_empty());
            assert_eq!(
                account_status(&control, &account_id.to_string())
                    .await
                    .as_deref(),
                Some("provisioning"),
                "a skipped row must be untouched"
            );

            // Closing the rival session releases its lock.
            rival.close().await.expect("close rival session");

            let summary = run_reaper_pass(&control, &cluster, &ReaperPolicy::default()).await;
            assert_no_errors(&summary);
            assert_eq!(summary.stuck_resumed, vec![account_id.to_string()]);
            assert_eq!(
                account_status(&control, &account_id.to_string())
                    .await
                    .as_deref(),
                Some("active")
            );
        },
    )
    .await;
}

/// Same skip for the orphan arm, keyed on the account id embedded in the
/// database name.
#[tokio::test]
async fn contended_lock_defers_orphan_reclaim() {
    with_control_db("contended_lock_defers_orphan_reclaim", |url| async move {
        let (control, cluster) = setup(&url).await;
        let orphan_uuid = Uuid::new_v4();
        let orphan_db = tenant_db_name(orphan_uuid);
        create_database(&cluster.cluster_url, &orphan_db).await;

        with_db_guard(&cluster.cluster_url, &orphan_db, || async {
            let mut rival = PgConnection::connect(&url).await.expect("rival session");
            let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                .bind(reaper_lock_key(&orphan_uuid.to_string()))
                .fetch_one(&mut rival)
                .await
                .expect("rival takes lock");
            assert!(got);

            let summary = run_reaper_pass(&control, &cluster, &ReaperPolicy::default()).await;
            assert_no_errors(&summary);
            assert_eq!(summary.orphan_dbs_skipped_locked, vec![orphan_db.clone()]);
            assert!(summary.orphan_dbs_dropped.is_empty());
            assert!(
                database_exists(&cluster.cluster_url, &orphan_db).await,
                "skipped orphan must still exist"
            );

            rival.close().await.expect("close rival session");

            let summary = run_reaper_pass(&control, &cluster, &ReaperPolicy::default()).await;
            assert_no_errors(&summary);
            assert_eq!(summary.orphan_dbs_dropped, vec![orphan_db.clone()]);
            assert!(!database_exists(&cluster.cluster_url, &orphan_db).await);
        })
        .await;
    })
    .await;
}

/// Two passes racing over the same stuck row resume it exactly once: the
/// loser either skips on lock contention or finds the row already settled
/// under its lock. Whatever the interleaving, the work happens once.
#[tokio::test]
async fn simultaneous_passes_resume_exactly_once() {
    with_control_db(
        "simultaneous_passes_resume_exactly_once",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            let account_id = Uuid::new_v4();
            seed_provisioning_row(&control, account_id, "kenny@example.com", "raced", 10).await;

            let policy = ReaperPolicy::default();
            let (a, b) = tokio::join!(
                run_reaper_pass(&control, &cluster, &policy),
                run_reaper_pass(&control, &cluster, &policy),
            );
            assert_no_errors(&a);
            assert_no_errors(&b);
            assert_eq!(
                a.stuck_resumed.len() + b.stuck_resumed.len(),
                1,
                "exactly one pass may resume; got a={a:?} b={b:?}"
            );
            assert!(a.stuck_rolled_back.is_empty() && b.stuck_rolled_back.is_empty());
            assert_eq!(
                account_status(&control, &account_id.to_string())
                    .await
                    .as_deref(),
                Some("active")
            );
            assert_eq!(
                count(
                    &control,
                    "SELECT COUNT(*) FROM account_databases WHERE account_id = $1",
                    &account_id.to_string(),
                )
                .await,
                1,
                "no duplicate mapping rows from the race"
            );
        },
    )
    .await;
}

/// The per-pass resume cap defers surplus rows without wedging: with the
/// cap at 1 and two unresumable rows, the first pass handles one and defers
/// the other; the second pass finishes the job.
#[tokio::test]
async fn resume_cap_defers_surplus_rows_across_passes() {
    with_control_db(
        "resume_cap_defers_surplus_rows_across_passes",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            // Both rows have corrupt emails, so both take the (slow-path
            // exercising) rollback branch.
            seed_provisioning_row(&control, Uuid::new_v4(), "bad-email-1", "capped-a", 10).await;
            seed_provisioning_row(&control, Uuid::new_v4(), "bad-email-2", "capped-b", 10).await;

            let policy = ReaperPolicy {
                max_resumes_per_pass: 1,
                ..ReaperPolicy::default()
            };
            let first = run_reaper_pass(&control, &cluster, &policy).await;
            assert_no_errors(&first);
            assert_eq!(first.stuck_rolled_back.len(), 1);
            assert_eq!(
                first.stuck_deferred.len(),
                1,
                "surplus row deferred, not dropped"
            );

            let second = run_reaper_pass(&control, &cluster, &policy).await;
            assert_no_errors(&second);
            assert_eq!(second.stuck_rolled_back.len(), 1);
            assert!(second.stuck_deferred.is_empty());

            assert_eq!(
                count(
                    &control,
                    "SELECT COUNT(*) FROM accounts WHERE status = $1",
                    "provisioning",
                )
                .await,
                0,
                "both stuck rows handled across the two passes"
            );
        },
    )
    .await;
}

/// Rows found already settled under their lock must NOT consume the resume
/// cap. Three stale rows, cap 2: while the oldest row's (real, slow) resume
/// runs, a saboteur settles the middle row by activating it — the pass must
/// spend its remaining budget on the youngest row, not defer it because a
/// settled row was charged.
#[tokio::test]
async fn settled_rows_do_not_consume_the_resume_cap() {
    with_control_db(
        "settled_rows_do_not_consume_the_resume_cap",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            // Processing order is created_at ascending: oldest first.
            let oldest = Uuid::new_v4();
            seed_provisioning_row(&control, oldest, "first@example.com", "cap-first", 30).await;
            let middle = Uuid::new_v4();
            seed_provisioning_row(&control, middle, "settled@example.com", "cap-settled", 20).await;
            let youngest = Uuid::new_v4();
            seed_provisioning_row(&control, youngest, "last@example.com", "cap-last", 10).await;

            let policy = ReaperPolicy {
                max_resumes_per_pass: 2,
                // The saboteur leaves `middle` active with no mapping row —
                // keep the interrupted-deletion arm away from it.
                deletion_recovery_grace: Duration::from_secs(3600),
                ..ReaperPolicy::default()
            };
            let oldest_db = tenant_db_name(oldest);
            let pass = run_reaper_pass(&control, &cluster, &policy);
            let saboteur = async {
                // The oldest row's resume signals progress by creating its
                // tenant database; migrations leave a wide window in which
                // to settle the middle row before the pass reaches it.
                let deadline = std::time::Instant::now() + Duration::from_secs(30);
                while !database_exists(&cluster.cluster_url, &oldest_db).await {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "oldest row's tenant database never appeared"
                    );
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                sqlx::query("UPDATE accounts SET status = 'active' WHERE id = $1")
                    .bind(middle.to_string())
                    .execute(control.pool())
                    .await
                    .expect("settle the middle row");
            };
            let (summary, ()) = tokio::join!(pass, saboteur);

            assert_no_errors(&summary);
            assert_eq!(
                summary.stuck_resumed,
                vec![oldest.to_string(), youngest.to_string()],
                "both real rows resumed in one pass: {summary:?}"
            );
            assert!(
                summary.stuck_deferred.is_empty(),
                "a settled row must not crowd real work out of the cap: {summary:?}"
            );
        },
    )
    .await;
}
