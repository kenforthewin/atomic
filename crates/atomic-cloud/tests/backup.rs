//! Logical-backup integration tests (plan: "Backups & disaster recovery").
//!
//! Two kinds of test live here:
//!
//! - **Local-store + query tests** — Postgres-gated like the rest of the
//!   suite (control plane on a throwaway database), but needing no external
//!   tools. They exercise the [`LocalFileSystemStore`] round trip and the
//!   control-plane backup queries/ledger.
//! - **dump → restore → verify** — additionally needs `pg_dump`/`pg_restore`
//!   on PATH. They provision a throwaway tenant, write a recognizable atom,
//!   dump it, restore into a NEW database, and assert the atom rehydrated.
//!   When the binaries are absent (a bare CI image) they skip with a clear
//!   message — mirroring the PG-gating idiom — and run for real locally,
//!   where the pgvector/pg16 cluster lives.
//!
//! Dump files are written under a unique temp dir (the local store's base)
//! and cleaned up; the restored tenant database is dropped by a guard. Never
//! a dump file or a stray database left behind.

mod support;

use std::sync::Arc;

use atomic_cloud::backup::backup_tools_available;
use atomic_cloud::{
    delete_account, dump_tenant_database, dumps_for_account, list_active_tenant_databases,
    provision_account, recent_backup_runs, record_backup_failure, record_backup_success,
    restore_database, stale_tenant_backups, start_backup_run, tenant_backup_status, tenant_db_name,
    BackupStore, ClusterConfig, ControlPlane, DumpConnection, LocalFileSystemStore, ManagedKeys,
    NewAccount,
};
use atomic_core::{CreateAtomRequest, DatabaseManager};
use support::{with_control_db, with_db_guard};

/// Migrated control plane + a cluster config pointing at the test cluster.
async fn setup(control_url: &str) -> (ControlPlane, ClusterConfig) {
    let control = ControlPlane::connect(
        control_url,
        atomic_cloud::control_plane::DEFAULT_CONTROL_POOL_MAX_CONNECTIONS,
    )
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

/// A unique temp dir for a test's local backup store, removed on drop.
fn temp_store() -> (tempfile::TempDir, Arc<dyn BackupStore>) {
    let dir = tempfile::tempdir().expect("create temp backup dir");
    let store: Arc<dyn BackupStore> = Arc::new(LocalFileSystemStore::new(dir.path().to_path_buf()));
    (dir, store)
}

// ==================== Local store round trip ====================

#[tokio::test]
async fn local_store_put_get_list_exists_round_trip() {
    // No cluster needed — the local store is pure filesystem and always runs.
    let dir = tempfile::tempdir().expect("temp dir");
    let store = LocalFileSystemStore::new(dir.path().to_path_buf());

    let key_a = "backups/2026-06-09/acct_aaaaaaaaaaaaaaaaaaaaaaaaaa.dump";
    let key_b = "backups/2026-06-09/control.dump";
    let key_c = "backups/final/11111111-2222-3333-4444-555555555555-20260609T031400Z.dump";

    // exists is false before any write.
    assert!(!store.exists(key_a).await.unwrap());

    store.put(key_a, b"alpha-bytes".to_vec()).await.unwrap();
    store.put(key_b, b"control-bytes".to_vec()).await.unwrap();
    store.put(key_c, b"final-bytes".to_vec()).await.unwrap();

    // get round-trips exactly.
    assert_eq!(store.get(key_a).await.unwrap(), b"alpha-bytes");
    assert_eq!(store.get(key_b).await.unwrap(), b"control-bytes");

    // exists is true after write, false for an absent key.
    assert!(store.exists(key_a).await.unwrap());
    assert!(!store
        .exists("backups/2026-06-09/missing.dump")
        .await
        .unwrap());

    // list by prefix is exact and prefix-scoped.
    let dated = store.list("backups/2026-06-09/").await.unwrap();
    assert_eq!(dated.len(), 2, "two keys under the date prefix: {dated:?}");
    assert!(dated.iter().any(|k| k == key_a));
    assert!(dated.iter().any(|k| k == key_b));
    let finals = store.list("backups/final/").await.unwrap();
    assert_eq!(finals, vec![key_c.to_string()]);
    let all = store.list("backups/").await.unwrap();
    assert_eq!(all.len(), 3);

    // get on a missing key is an error, never an empty success.
    assert!(store.get("backups/nope.dump").await.is_err());

    // overwrite is idempotent (a re-run of a day's pass).
    store.put(key_a, b"alpha-v2".to_vec()).await.unwrap();
    assert_eq!(store.get(key_a).await.unwrap(), b"alpha-v2");

    // Empty store lists nothing (a fresh base dir need not pre-exist).
    let empty_dir = tempfile::tempdir().expect("temp dir");
    let empty = LocalFileSystemStore::new(empty_dir.path().join("not-created-yet"));
    assert!(empty.list("backups/").await.unwrap().is_empty());
}

// ==================== Control-plane backup queries / ledger ==============

#[tokio::test]
async fn backup_status_and_ledger_round_trip() {
    with_control_db("backup_status_and_ledger_round_trip", |url| async move {
        let (control, cluster) = setup(&url).await;

        let acct = provision_account(
            &control,
            &cluster,
            &ManagedKeys::Disabled,
            NewAccount {
                email: "ledger@example.com".into(),
                subdomain: "ledger".into(),
            },
        )
        .await
        .expect("provision");

        // A freshly provisioned active tenant is listed with no backup yet.
        let targets = list_active_tenant_databases(&control).await.unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].account_id, acct.account_id);
        assert_eq!(targets[0].db_name, acct.db_name);

        // It is stale (never backed up) only once older than the horizon —
        // with a zero horizon, a never-backed-up active tenant trips it.
        let now = chrono::Utc::now();
        let stale = stale_tenant_backups(&control, std::time::Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(
            stale.len(),
            1,
            "never-backed-up tenant is stale at horizon 0"
        );
        assert!(stale[0].last_backup_at.is_none());

        // Recording success clears staleness and stamps last_backup_at.
        record_backup_success(&control, &acct.account_id, &acct.db_name, now)
            .await
            .unwrap();
        let fresh = stale_tenant_backups(&control, std::time::Duration::from_secs(36 * 60 * 60))
            .await
            .unwrap();
        assert!(fresh.is_empty(), "a just-backed-up tenant is not stale");

        // A failure records the error but does NOT reset last_backup_at — the
        // monitor must keep seeing the last *success*, so a tenant whose
        // backups start failing still trips the alert by its stale success.
        record_backup_failure(
            &control,
            &acct.account_id,
            &acct.db_name,
            "pg_dump: boom",
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        let (last_at, last_err): (Option<chrono::DateTime<chrono::Utc>>, Option<String>) =
            sqlx::query_as(
                "SELECT last_backup_at, last_backup_error FROM account_databases \
                 WHERE account_id = $1",
            )
            .bind(&acct.account_id)
            .fetch_one(control.pool())
            .await
            .unwrap();
        assert!(last_at.is_some(), "failure must not clear last success");
        assert_eq!(last_err.as_deref(), Some("pg_dump: boom"));

        // The run ledger records start + finish.
        let run_id = start_backup_run(&control, "nightly").await.unwrap();
        atomic_cloud::finish_backup_run(&control, &run_id, 3, 2, 1)
            .await
            .unwrap();
        let (kind, total, succeeded, failed): (String, i32, i32, i32) =
            sqlx::query_as("SELECT kind, total, succeeded, failed FROM backup_runs WHERE id = $1")
                .bind(&run_id)
                .fetch_one(control.pool())
                .await
                .unwrap();
        assert_eq!(kind, "nightly");
        assert_eq!((total, succeeded, failed), (3, 2, 1));
    })
    .await;
}

// ==================== The nightly pass ====================

#[tokio::test]
async fn nightly_pass_backs_up_every_tenant_plus_control() {
    if !backup_tools_available().await {
        eprintln!(
            "nightly_pass_backs_up_every_tenant_plus_control: skipping \
             (pg_dump/pg_restore not on PATH)"
        );
        return;
    }
    with_control_db(
        "nightly_pass_backs_up_every_tenant_plus_control",
        |url| async move {
            let (control, cluster) = setup(&url).await;

            // Two active tenants.
            let mut accts = Vec::new();
            for (email, sub) in [("a@example.com", "passa"), ("b@example.com", "passb")] {
                accts.push(
                    provision_account(
                        &control,
                        &cluster,
                        &ManagedKeys::Disabled,
                        NewAccount {
                            email: email.into(),
                            subdomain: sub.into(),
                        },
                    )
                    .await
                    .expect("provision"),
                );
            }

            let (_dir, store) = temp_store();
            let config = atomic_cloud::BackupConfig::default();
            let now = chrono::Utc::now();
            let summary =
                atomic_cloud::run_backup_pass(&control, &cluster, &store, &config, now).await;

            // Every tenant + the control plane were backed up; no errors.
            assert_eq!(
                summary.tenants_backed_up.len(),
                2,
                "both tenants backed up: {summary:?}"
            );
            assert!(summary.control_backed_up, "control plane backed up");
            assert!(summary.errors.is_empty(), "no errors: {summary:?}");
            assert!(summary.tenants_failed.is_empty());

            // The dumps physically landed under the day's prefix (two tenant
            // dumps + one control dump), each a real PGDMP blob.
            let date_prefix = format!("backups/{}/", now.format("%Y-%m-%d"));
            let keys = store.list(&date_prefix).await.unwrap();
            assert_eq!(keys.len(), 3, "two tenants + control: {keys:?}");
            for key in &keys {
                let bytes = store.get(key).await.unwrap();
                assert_eq!(&bytes[..5], b"PGDMP", "{key} is a custom-format dump");
            }

            // last_backup_at was stamped on every tenant (so the next pass
            // wouldn't redo them, and staleness clears).
            for acct in &accts {
                let last: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
                    "SELECT last_backup_at FROM account_databases WHERE account_id = $1",
                )
                .bind(&acct.account_id)
                .fetch_one(control.pool())
                .await
                .unwrap();
                assert!(last.is_some(), "tenant {} stamped", acct.account_id);
            }

            // The run ledger recorded the pass.
            let (kind, total, succeeded, failed): (String, i32, i32, i32) = sqlx::query_as(
                "SELECT kind, total, succeeded, failed FROM backup_runs \
                 ORDER BY started_at DESC LIMIT 1",
            )
            .fetch_one(control.pool())
            .await
            .unwrap();
            assert_eq!(kind, "nightly");
            assert_eq!((total, succeeded, failed), (2, 2, 0));
        },
    )
    .await;
}

/// A [`BackupStore`] that delegates to an inner local store but fails `put`
/// for one specific key substring — used to prove a single tenant's dump
/// failure mid-pass is recorded WITHOUT aborting the rest of the fleet.
struct PutFailsForKey {
    inner: Arc<dyn BackupStore>,
    fail_substring: String,
}

#[async_trait::async_trait]
impl BackupStore for PutFailsForKey {
    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<(), atomic_cloud::CloudError> {
        if key.contains(&self.fail_substring) {
            return Err(atomic_cloud::CloudError::BackupStore(format!(
                "simulated upload failure for {key}"
            )));
        }
        self.inner.put(key, bytes).await
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>, atomic_cloud::CloudError> {
        self.inner.get(key).await
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>, atomic_cloud::CloudError> {
        self.inner.list(prefix).await
    }
    async fn exists(&self, key: &str) -> Result<bool, atomic_cloud::CloudError> {
        self.inner.exists(key).await
    }
}

/// One tenant whose dump upload fails is recorded as failed and surfaced in the
/// summary, but the OTHER tenant and the control plane still back up — a broken
/// tenant must never starve its neighbors (plan: "one tenant failing NEVER
/// aborts the pass"). The `backup_runs` ledger reflects the split (succeeded=1,
/// failed=1), and the failed tenant's row carries `last_backup_error` while its
/// `last_backup_at` is left untouched (the staleness monitor must keep seeing
/// the last success, never be reset by a failure).
#[tokio::test]
async fn one_tenant_failure_does_not_abort_the_pass() {
    if !backup_tools_available().await {
        eprintln!(
            "one_tenant_failure_does_not_abort_the_pass: skipping \
             (pg_dump/pg_restore not on PATH)"
        );
        return;
    }
    with_control_db(
        "one_tenant_failure_does_not_abort_the_pass",
        |url| async move {
            let (control, cluster) = setup(&url).await;

            // Two active tenants: "good" backs up, "bad" has its upload rejected.
            let good = provision_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                NewAccount {
                    email: "good@example.com".into(),
                    subdomain: "goodten".into(),
                },
            )
            .await
            .expect("provision good");
            let bad = provision_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                NewAccount {
                    email: "bad@example.com".into(),
                    subdomain: "badten".into(),
                },
            )
            .await
            .expect("provision bad");

            // The store fails `put` only for the bad tenant's db_name in the key.
            let (_dir, inner) = temp_store();
            let store: Arc<dyn BackupStore> = Arc::new(PutFailsForKey {
                inner: Arc::clone(&inner),
                fail_substring: bad.db_name.clone(),
            });

            let now = chrono::Utc::now();
            let config = atomic_cloud::BackupConfig::default();
            let summary =
                atomic_cloud::run_backup_pass(&control, &cluster, &store, &config, now).await;

            // The good tenant + control plane succeeded; the bad tenant failed,
            // but the pass as a whole did not abort.
            assert_eq!(
                summary.tenants_backed_up,
                vec![good.account_id.clone()],
                "only the good tenant is backed up: {summary:?}"
            );
            assert_eq!(
                summary.tenants_failed,
                vec![bad.account_id.clone()],
                "the bad tenant is recorded failed: {summary:?}"
            );
            assert!(summary.control_backed_up, "control still backs up");
            assert_eq!(summary.errors.len(), 1, "one per-tenant error: {summary:?}");

            // The dumps physically present: good tenant + control under the day's
            // prefix (the bad tenant's upload was rejected, so it is absent).
            let date_prefix = format!("backups/{}/", now.format("%Y-%m-%d"));
            let keys = inner.list(&date_prefix).await.unwrap();
            assert_eq!(keys.len(), 2, "good tenant + control only: {keys:?}");
            assert!(keys.iter().any(|k| k.contains(&good.db_name)));
            assert!(keys.iter().any(|k| k.ends_with("control.dump")));
            assert!(
                !keys.iter().any(|k| k.contains(&bad.db_name)),
                "the failed tenant's dump must NOT be present"
            );

            // The good tenant was stamped; the bad tenant carries the error and
            // was NOT stamped (last_backup_at stays NULL — it never succeeded).
            let (good_at, good_err): (Option<chrono::DateTime<chrono::Utc>>, Option<String>) =
                sqlx::query_as(
                    "SELECT last_backup_at, last_backup_error FROM account_databases \
                     WHERE account_id = $1",
                )
                .bind(&good.account_id)
                .fetch_one(control.pool())
                .await
                .unwrap();
            assert!(good_at.is_some(), "good tenant stamped");
            assert!(good_err.is_none(), "good tenant has no error");

            let (bad_at, bad_err): (Option<chrono::DateTime<chrono::Utc>>, Option<String>) =
                sqlx::query_as(
                    "SELECT last_backup_at, last_backup_error FROM account_databases \
                     WHERE account_id = $1",
                )
                .bind(&bad.account_id)
                .fetch_one(control.pool())
                .await
                .unwrap();
            assert!(
                bad_at.is_none(),
                "the failed tenant must NOT be stamped (it never succeeded)"
            );
            assert!(
                bad_err.is_some(),
                "the failed tenant must carry last_backup_error"
            );

            // The ledger reflects the split: two attempted, one succeeded, one
            // failed.
            let (total, succeeded, failed): (i32, i32, i32) = sqlx::query_as(
                "SELECT total, succeeded, failed FROM backup_runs \
                 ORDER BY started_at DESC LIMIT 1",
            )
            .fetch_one(control.pool())
            .await
            .unwrap();
            assert_eq!((total, succeeded, failed), (2, 1, 1), "ledger split");
        },
    )
    .await;
}

/// Two pods cannot dump the same tenant at once: while a held per-account
/// advisory lock simulates another pod mid-dump, a concurrent pass skips that
/// tenant (observable in [`BackupSummary::tenants_skipped_locked`]) rather than
/// dumping it twice. The same per-account lock the reaper takes
/// ([`try_account_advisory_lock`]) is what makes the backup pass cross-pod safe
/// (plan: "so two pods do not dump the same tenant at once").
#[tokio::test]
async fn concurrent_pass_skips_a_locked_tenant() {
    if !backup_tools_available().await {
        eprintln!(
            "concurrent_pass_skips_a_locked_tenant: skipping \
             (pg_dump/pg_restore not on PATH)"
        );
        return;
    }
    with_control_db("concurrent_pass_skips_a_locked_tenant", |url| async move {
        let (control, cluster) = setup(&url).await;

        // Two active tenants. We hold the lock for "locked" to stand in for a
        // sibling pod that is mid-dump, and leave "free" unlocked.
        let locked = provision_account(
            &control,
            &cluster,
            &ManagedKeys::Disabled,
            NewAccount {
                email: "locked@example.com".into(),
                subdomain: "lockedten".into(),
            },
        )
        .await
        .expect("provision locked");
        let free = provision_account(
            &control,
            &cluster,
            &ManagedKeys::Disabled,
            NewAccount {
                email: "free@example.com".into(),
                subdomain: "freeten".into(),
            },
        )
        .await
        .expect("provision free");

        // Take and HOLD the locked tenant's advisory lock for the duration of
        // the pass — the connection owning the session-level lock must outlive
        // run_backup_pass, so the pass's own try-lock returns None and skips.
        let held = atomic_cloud::reaper::try_account_advisory_lock(&control, &locked.account_id)
            .await
            .expect("take advisory lock")
            .expect("lock is free before the pass");

        let (_dir, store) = temp_store();
        let now = chrono::Utc::now();
        let config = atomic_cloud::BackupConfig::default();
        let summary = atomic_cloud::run_backup_pass(&control, &cluster, &store, &config, now).await;

        // The locked tenant was skipped (not dumped, not failed); the free
        // tenant backed up normally; the control plane backed up.
        assert_eq!(
            summary.tenants_skipped_locked,
            vec![locked.account_id.clone()],
            "the contended tenant is skipped, not double-dumped: {summary:?}"
        );
        assert_eq!(
            summary.tenants_backed_up,
            vec![free.account_id.clone()],
            "the free tenant still backs up: {summary:?}"
        );
        assert!(summary.tenants_failed.is_empty(), "a skip is not a failure");
        assert!(summary.control_backed_up);

        // The skipped tenant has no dump on disk and was not stamped (a skip is
        // a no-op for that tenant — the next pass will reach it).
        let date_prefix = format!("backups/{}/", now.format("%Y-%m-%d"));
        let keys = store.list(&date_prefix).await.unwrap();
        assert!(
            !keys.iter().any(|k| k.contains(&locked.db_name)),
            "the locked tenant must have no dump: {keys:?}"
        );
        let locked_at: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
            "SELECT last_backup_at FROM account_databases WHERE account_id = $1",
        )
        .bind(&locked.account_id)
        .fetch_one(control.pool())
        .await
        .unwrap();
        assert!(
            locked_at.is_none(),
            "a skipped tenant is not stamped — the next pass reaches it"
        );

        // Release the held lock (end the session) so cleanup can drop the DBs.
        let _ = sqlx::Connection::close(held).await;
    })
    .await;
}

// ==================== Per-dump timeout (issue 1) ====================

/// A tenant whose `pg_dump` overruns the per-dump timeout is killed and
/// recorded as a typed timeout failure — the pass does NOT hang on it and the
/// OTHER tenant + control plane still back up (adversarial-review issue 1). We
/// inject the timeout at the runner seam by configuring a 1ms budget: no real
/// `pg_dump` of a populated tenant finishes that fast, so it deterministically
/// times out, while the rest of the pass proceeds.
#[tokio::test]
async fn pass_records_a_timeout_failure_and_keeps_going() {
    if !backup_tools_available().await {
        eprintln!(
            "pass_records_a_timeout_failure_and_keeps_going: skipping \
             (pg_dump/pg_restore not on PATH)"
        );
        return;
    }
    with_control_db(
        "pass_records_a_timeout_failure_and_keeps_going",
        |url| async move {
            let (control, cluster) = setup(&url).await;

            // Two active tenants — both will time out under a 1ms budget, but
            // the pass must run to completion (control plane backed up) and
            // record EACH as a failure rather than hanging on the first.
            let mut accts = Vec::new();
            for (email, sub) in [("t1@example.com", "toa"), ("t2@example.com", "tob")] {
                accts.push(
                    provision_account(
                        &control,
                        &cluster,
                        &ManagedKeys::Disabled,
                        NewAccount {
                            email: email.into(),
                            subdomain: sub.into(),
                        },
                    )
                    .await
                    .expect("provision"),
                );
            }

            let (_dir, store) = temp_store();
            let now = chrono::Utc::now();
            // 1ms per-dump budget: pg_dump cannot complete; it times out and is
            // killed. The control-plane dump uses the same budget and likewise
            // times out — that's fine, the point is the pass returns promptly
            // and records typed failures instead of hanging forever.
            let config = atomic_cloud::BackupConfig {
                backup_timeout: std::time::Duration::from_millis(1),
                ..atomic_cloud::BackupConfig::default()
            };

            let started = std::time::Instant::now();
            let summary =
                atomic_cloud::run_backup_pass(&control, &cluster, &store, &config, now).await;
            let elapsed = started.elapsed();

            // The pass returned promptly — no hung child wedged it. (A real
            // dump of two tenants takes well under this; the point is it did
            // not block on a 60s-style stall.)
            assert!(
                elapsed < std::time::Duration::from_secs(30),
                "a timing-out pass must not hang: took {elapsed:?}"
            );

            // BOTH tenants recorded failed; none reported backed up. One
            // tenant's timeout did not abort the pass before the other.
            assert_eq!(
                summary.tenants_failed.len(),
                2,
                "both tenants time out and are recorded failed: {summary:?}"
            );
            assert!(
                summary.tenants_backed_up.is_empty(),
                "no tenant completed within 1ms: {summary:?}"
            );

            // The failure surfaced as a timeout on each tenant's row, and
            // last_backup_at was left untouched (a failure must not look fresh).
            for acct in &accts {
                let (last_at, last_err): (Option<chrono::DateTime<chrono::Utc>>, Option<String>) =
                    sqlx::query_as(
                        "SELECT last_backup_at, last_backup_error FROM account_databases \
                     WHERE account_id = $1",
                    )
                    .bind(&acct.account_id)
                    .fetch_one(control.pool())
                    .await
                    .unwrap();
                assert!(last_at.is_none(), "a timed-out tenant is not stamped fresh");
                let err = last_err.expect("a timed-out tenant carries an error");
                assert!(
                    err.contains("timed out"),
                    "the recorded error names the timeout: {err}"
                );
            }
        },
    )
    .await;
}

// ============== Cap / deferral + starvation ordering (issues 4 & 5) ==========

/// `max_backups_per_pass` caps a pass: exactly the cap is dumped and the rest
/// are deferred (most-overdue-first), and a second pass picks up the deferred
/// ones (adversarial-review issue 4). With three tenants and a cap of two, pass
/// one dumps two and defers one; pass two reaches the deferred tenant.
#[tokio::test]
async fn cap_defers_excess_and_next_pass_reaches_them() {
    if !backup_tools_available().await {
        eprintln!("cap_defers_excess_and_next_pass_reaches_them: skipping (no pg_dump)");
        return;
    }
    with_control_db(
        "cap_defers_excess_and_next_pass_reaches_them",
        |url| async move {
            let (control, cluster) = setup(&url).await;

            let mut accts = Vec::new();
            for (email, sub) in [
                ("c1@example.com", "capa"),
                ("c2@example.com", "capb"),
                ("c3@example.com", "capc"),
            ] {
                accts.push(
                    provision_account(
                        &control,
                        &cluster,
                        &ManagedKeys::Disabled,
                        NewAccount {
                            email: email.into(),
                            subdomain: sub.into(),
                        },
                    )
                    .await
                    .expect("provision"),
                );
            }

            let (_dir, store) = temp_store();
            let config = atomic_cloud::BackupConfig {
                max_backups_per_pass: 2,
                ..atomic_cloud::BackupConfig::default()
            };

            // Pass one: exactly two dumped, exactly one deferred.
            let now1 = chrono::Utc::now();
            let s1 = atomic_cloud::run_backup_pass(&control, &cluster, &store, &config, now1).await;
            assert_eq!(s1.tenants_backed_up.len(), 2, "cap dumps exactly 2: {s1:?}");
            assert_eq!(s1.tenants_deferred.len(), 1, "one deferred: {s1:?}");
            assert!(s1.control_backed_up);

            // The deferred tenant is the one NOT backed up — and it was never
            // stamped, so the most-overdue ordering surfaces it next pass.
            let deferred_id = s1.tenants_deferred[0].clone();
            assert!(
                !s1.tenants_backed_up.contains(&deferred_id),
                "the deferred tenant is not also reported backed up"
            );

            // Pass two (slightly later): the deferred tenant — now the most
            // overdue (NULL last_backup_at sorts first) — is reached.
            let now2 = now1 + chrono::Duration::seconds(1);
            let s2 = atomic_cloud::run_backup_pass(&control, &cluster, &store, &config, now2).await;
            assert!(
                s2.tenants_backed_up.contains(&deferred_id),
                "the previously deferred tenant is reached next pass: {s2:?}"
            );

            // After two passes every tenant has a dump on disk.
            for acct in &accts {
                let last: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
                    "SELECT last_backup_at FROM account_databases WHERE account_id = $1",
                )
                .bind(&acct.account_id)
                .fetch_one(control.pool())
                .await
                .unwrap();
                assert!(
                    last.is_some(),
                    "tenant {} eventually backed up",
                    acct.account_id
                );
            }
        },
    )
    .await;
}

/// A persistently-FAILING tenant must not starve a healthy-but-due one under a
/// small cap across passes (adversarial-review issue 5). Ordering by the most
/// recent *attempt* (not last success) means a tenant whose dump keeps failing
/// sinks behind a never-attempted/healthy-but-due tenant once it has been
/// tried, instead of floating to the front of every pass forever.
#[tokio::test]
async fn persistently_failing_tenant_does_not_starve_a_healthy_one() {
    if !backup_tools_available().await {
        eprintln!(
            "persistently_failing_tenant_does_not_starve_a_healthy_one: skipping (no pg_dump)"
        );
        return;
    }
    with_control_db(
        "persistently_failing_tenant_does_not_starve_a_healthy_one",
        |url| async move {
            let (control, cluster) = setup(&url).await;

            let broken = provision_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                NewAccount {
                    email: "broken@example.com".into(),
                    subdomain: "brokenten".into(),
                },
            )
            .await
            .expect("provision broken");
            let healthy = provision_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                NewAccount {
                    email: "healthy@example.com".into(),
                    subdomain: "healthyten".into(),
                },
            )
            .await
            .expect("provision healthy");

            // A store that always fails the broken tenant's upload; the healthy
            // tenant uploads fine.
            let (_dir, inner) = temp_store();
            let store: Arc<dyn BackupStore> = Arc::new(PutFailsForKey {
                inner: Arc::clone(&inner),
                fail_substring: broken.db_name.clone(),
            });

            // Cap of ONE: only one tenant is attempted per pass. If ordering
            // floated the broken tenant first every pass, the healthy tenant
            // would never be reached. Run two passes and assert the healthy
            // tenant DID get backed up.
            let config = atomic_cloud::BackupConfig {
                max_backups_per_pass: 1,
                ..atomic_cloud::BackupConfig::default()
            };

            let now1 = chrono::Utc::now();
            let _ = atomic_cloud::run_backup_pass(&control, &cluster, &store, &config, now1).await;
            // After pass one, the broken tenant has a last_backup_attempt_at
            // (failure stamped) but no last_backup_at; the healthy tenant has
            // neither yet → it sorts first next pass.
            let now2 = now1 + chrono::Duration::seconds(1);
            let _ = atomic_cloud::run_backup_pass(&control, &cluster, &store, &config, now2).await;

            let healthy_at: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
                "SELECT last_backup_at FROM account_databases WHERE account_id = $1",
            )
            .bind(&healthy.account_id)
            .fetch_one(control.pool())
            .await
            .unwrap();
            assert!(
                healthy_at.is_some(),
                "the healthy-but-due tenant must be reached despite the broken one's repeated \
                 failures (no starvation)"
            );

            // And the broken tenant kept failing (still no success), proving it
            // was genuinely contending the front of the queue.
            let broken_at: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
                "SELECT last_backup_at FROM account_databases WHERE account_id = $1",
            )
            .bind(&broken.account_id)
            .fetch_one(control.pool())
            .await
            .unwrap();
            assert!(broken_at.is_none(), "the broken tenant never succeeded");
        },
    )
    .await;
}

// ==================== Abandoned-run finalizer (issue 6) ====================

/// A pod killed mid-pass leaves a `backup_runs` row `finished_at IS NULL`
/// forever; [`finalize_abandoned_backup_runs`] marks rows older than the
/// horizon `'abandoned'` so status doesn't show a perpetually in-flight pass
/// (adversarial-review issue 6). A genuinely recent in-flight row is left
/// alone.
#[tokio::test]
async fn finalize_abandoned_backup_runs_marks_only_stale_in_flight() {
    with_control_db(
        "finalize_abandoned_backup_runs_marks_only_stale_in_flight",
        |url| async move {
            let (control, _cluster) = setup(&url).await;

            // A stale in-flight row (started 7h ago, never finished).
            let stale_id = start_backup_run(&control, "nightly").await.unwrap();
            sqlx::query(
                "UPDATE backup_runs SET started_at = NOW() - INTERVAL '7 hours' WHERE id = $1",
            )
            .bind(&stale_id)
            .execute(control.pool())
            .await
            .unwrap();

            // A fresh in-flight row (just started) that must NOT be touched.
            let fresh_id = start_backup_run(&control, "nightly").await.unwrap();

            let finalized = atomic_cloud::finalize_abandoned_backup_runs(
                &control,
                std::time::Duration::from_secs(6 * 60 * 60),
            )
            .await
            .unwrap();
            assert_eq!(finalized, 1, "exactly the one stale row is finalized");

            let runs = recent_backup_runs(&control, 10).await.unwrap();
            let stale = runs.iter().find(|r| r.id == stale_id).unwrap();
            assert_eq!(stale.status.as_deref(), Some("abandoned"));
            assert!(
                stale.finished_at.is_some(),
                "abandoned row gets a finished_at"
            );

            let fresh = runs.iter().find(|r| r.id == fresh_id).unwrap();
            assert_eq!(fresh.status.as_deref(), Some("running"));
            assert!(fresh.finished_at.is_none(), "the fresh row stays in-flight");
        },
    )
    .await;
}

// ============ Delete vs backup mutual exclusion (issues 2 & 3) ===============

/// A delete and a concurrent backup pass on the SAME tenant serialize on the
/// per-account advisory lock (adversarial-review issue 2). Holding the lock
/// (standing in for a backup pass mid-dump), a `DeleteLock::Acquire` delete
/// observes the contention and returns `CloudError::Busy` rather than racing a
/// `DROP DATABASE` against the live dump. With the lock released, the same
/// delete proceeds.
#[tokio::test]
async fn delete_in_acquire_mode_is_busy_while_a_backup_holds_the_lock() {
    if !backup_tools_available().await {
        eprintln!(
            "delete_in_acquire_mode_is_busy_while_a_backup_holds_the_lock: skipping (no pg_dump)"
        );
        return;
    }
    with_control_db(
        "delete_in_acquire_mode_is_busy_while_a_backup_holds_the_lock",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            let acct = provision_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                NewAccount {
                    email: "busy@example.com".into(),
                    subdomain: "busyten".into(),
                },
            )
            .await
            .expect("provision");

            // Hold the account's advisory lock — a backup pass mid-dump.
            let held = atomic_cloud::reaper::try_account_advisory_lock(&control, &acct.account_id)
                .await
                .expect("take lock")
                .expect("lock free");

            // A delete in Acquire mode cannot take the lock and, after its
            // brief retry budget, returns Busy. NOTHING is dropped (the guard
            // wraps the whole destructive window).
            let (_dir, store) = temp_store();
            let started = std::time::Instant::now();
            let err = delete_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                // No billing provider in tests: the subscription-cancel step is
                // skipped (DEL-1 `billing` is `None`), exactly as the CLI/reaper paths.
                None,
                atomic_cloud::BackupPolicy::Required(&store),
                atomic_cloud::DeleteLock::Acquire,
                &acct.account_id,
                atomic_cloud::DEFAULT_BACKUP_TIMEOUT,
            )
            .await
            .expect_err("delete must be Busy while a backup holds the lock");
            assert!(
                matches!(err, atomic_cloud::CloudError::Busy(_)),
                "expected Busy, got {err:?}"
            );
            // It waited the retry budget (~10s) rather than failing instantly or
            // hanging forever.
            assert!(
                started.elapsed() >= std::time::Duration::from_secs(1),
                "a Busy delete should have retried briefly, not failed instantly"
            );

            // The tenant database is untouched: the lock guarded the whole
            // destructive window, so the delete never started dropping.
            assert!(
                atomic_cloud::tenant_database_exists(&cluster, &acct.db_name)
                    .await
                    .unwrap(),
                "no drop happened under contention"
            );
            // No final dump was taken either (the delete never reached the dump).
            assert!(
                store.list("backups/final/").await.unwrap().is_empty(),
                "a Busy delete takes no final dump"
            );

            // Release the lock; the same delete now proceeds to completion.
            let _ = sqlx::Connection::close(held).await;
            delete_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                // No billing provider in tests: the subscription-cancel step is
                // skipped (DEL-1 `billing` is `None`), exactly as the CLI/reaper paths.
                None,
                atomic_cloud::BackupPolicy::Required(&store),
                atomic_cloud::DeleteLock::Acquire,
                &acct.account_id,
                atomic_cloud::DEFAULT_BACKUP_TIMEOUT,
            )
            .await
            .expect("delete succeeds once the lock is free");
            assert!(
                !atomic_cloud::tenant_database_exists(&cluster, &acct.db_name)
                    .await
                    .unwrap(),
                "the tenant database is dropped after the lock frees"
            );
            assert_eq!(
                store.list("backups/final/").await.unwrap().len(),
                1,
                "the successful delete took its final dump"
            );
        },
    )
    .await;
}

/// The reaper's interrupted-deletion arm ALREADY holds the per-account lock
/// when it calls `delete_account`; passing `DeleteLock::AlreadyHeld` must let
/// the deletion complete WITHOUT self-deadlocking on the lock it already owns
/// (adversarial-review issue 2). This drives `delete_account` directly under a
/// caller-held lock, exactly as the reaper does.
#[tokio::test]
async fn delete_in_already_held_mode_does_not_self_deadlock() {
    if !backup_tools_available().await {
        eprintln!("delete_in_already_held_mode_does_not_self_deadlock: skipping (no pg_dump)");
        return;
    }
    with_control_db(
        "delete_in_already_held_mode_does_not_self_deadlock",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            let acct = provision_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                NewAccount {
                    email: "held@example.com".into(),
                    subdomain: "heldten".into(),
                },
            )
            .await
            .expect("provision");

            // The caller (reaper) holds the lock first...
            let held = atomic_cloud::reaper::try_account_advisory_lock(&control, &acct.account_id)
                .await
                .expect("take lock")
                .expect("lock free");

            // ...then delete in AlreadyHeld mode. This must NOT try to
            // re-acquire (which would self-deadlock) and must complete the
            // deletion. A generous timeout proves "completes", not "hangs".
            let (_dir, store) = temp_store();
            let outcome = tokio::time::timeout(
                std::time::Duration::from_secs(60),
                delete_account(
                    &control,
                    &cluster,
                    &ManagedKeys::Disabled,
                    // No billing provider in tests: the subscription-cancel step is
                    // skipped (DEL-1 `billing` is `None`), exactly as the CLI/reaper paths.
                    None,
                    atomic_cloud::BackupPolicy::Required(&store),
                    atomic_cloud::DeleteLock::AlreadyHeld,
                    &acct.account_id,
                    atomic_cloud::DEFAULT_BACKUP_TIMEOUT,
                ),
            )
            .await
            .expect("AlreadyHeld delete must not deadlock/hang");
            outcome.expect("delete completes");

            // Release the caller's lock (the reaper does this after).
            let _ = sqlx::Connection::close(held).await;

            assert!(
                !atomic_cloud::tenant_database_exists(&cluster, &acct.db_name)
                    .await
                    .unwrap(),
                "the deletion completed and dropped the tenant database"
            );
            assert_eq!(
                store.list("backups/final/").await.unwrap().len(),
                1,
                "the final dump was taken before the drop"
            );
        },
    )
    .await;
}

/// An acknowledged-disabled backup policy deletes WITHOUT a final dump (the
/// explicit dev path, adversarial-review issue 3) — distinct from the
/// `Required` path which always dumps. This pins that the policy is an explicit
/// decision: there is no silent "forgot the store" drop, only `Required(store)`
/// (dumps) or `DisabledAcknowledged` (drops with a loud warn, no dump).
#[tokio::test]
async fn disabled_acknowledged_policy_drops_without_a_final_dump() {
    if !backup_tools_available().await {
        eprintln!("disabled_acknowledged_policy_drops_without_a_final_dump: skipping (no pg_dump)");
        return;
    }
    with_control_db(
        "disabled_acknowledged_policy_drops_without_a_final_dump",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            let acct = provision_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                NewAccount {
                    email: "nodump@example.com".into(),
                    subdomain: "nodumpten".into(),
                },
            )
            .await
            .expect("provision");

            let (_dir, store) = temp_store();
            delete_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                // No billing provider in tests: the subscription-cancel step is
                // skipped (DEL-1 `billing` is `None`), exactly as the CLI/reaper paths.
                None,
                atomic_cloud::BackupPolicy::DisabledAcknowledged,
                atomic_cloud::DeleteLock::Acquire,
                &acct.account_id,
                atomic_cloud::DEFAULT_BACKUP_TIMEOUT,
            )
            .await
            .expect("acknowledged-disabled delete proceeds");

            assert!(
                !atomic_cloud::tenant_database_exists(&cluster, &acct.db_name)
                    .await
                    .unwrap(),
                "the tenant database is dropped"
            );
            assert!(
                store.list("backups/final/").await.unwrap().is_empty(),
                "acknowledged-disabled takes NO final dump"
            );
        },
    )
    .await;
}

// ==================== Real dump → restore → verify ====================

#[tokio::test]
async fn dump_restore_round_trip_rehydrates_real_data() {
    if !backup_tools_available().await {
        eprintln!(
            "dump_restore_round_trip_rehydrates_real_data: skipping \
             (pg_dump/pg_restore not on PATH)"
        );
        return;
    }
    with_control_db(
        "dump_restore_round_trip_rehydrates_real_data",
        |url| async move {
            let (control, cluster) = setup(&url).await;

            // Provision a throwaway tenant.
            let acct = provision_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                NewAccount {
                    email: "restore@example.com".into(),
                    subdomain: "restoreme".into(),
                },
            )
            .await
            .expect("provision");

            // Write a recognizable atom through the tenant manager (inline
            // pipeline; no provider configured, so embedding reports a
            // structured error but the atom row persists — which is what the
            // dump must capture).
            const MARKER: &str = "backup-roundtrip-marker-7f3a9c";
            let source_url = "https://example.com/backup-roundtrip-source";
            let tenant_url = cluster.tenant_db_url(&acct.db_name).unwrap();
            let atom_id = {
                let manager = DatabaseManager::new_postgres(".", &tenant_url)
                    .await
                    .expect("open tenant");
                let core = manager.active_core().await.expect("active core");
                let created = core
                    .create_atom(
                        CreateAtomRequest {
                            content: format!("# Title\n\n{MARKER} body text"),
                            source_url: Some(source_url.to_string()),
                            ..Default::default()
                        },
                        |_| {},
                    )
                    .await
                    .expect("create atom")
                    .expect("atom inserted");
                drop(core);
                drop(manager);
                created.atom.id
            };

            // Dump the tenant database to bytes.
            let conn = DumpConnection::for_cluster(&cluster).unwrap();
            let dump =
                dump_tenant_database(&conn, &acct.db_name, atomic_cloud::DEFAULT_BACKUP_TIMEOUT)
                    .await
                    .expect("dump tenant database");
            assert!(!dump.is_empty(), "a real dump is non-empty");
            // pg_dump custom-format dumps start with the magic "PGDMP".
            assert_eq!(&dump[..5], b"PGDMP", "custom-format dump header");

            // Round-trip the bytes through the local store (put → get), so the
            // restore reads exactly what an upload would have stored.
            let (_dir, store) = temp_store();
            let key = "backups/test/tenant.dump";
            store.put(key, dump).await.expect("store dump");
            let from_store = store.get(key).await.expect("read dump back");

            // Restore into a FRESH tenant database (a new UUID's name). The
            // guard drops it whatever happens — it is NOT referenced by any
            // control-plane row, so the suite's own cleanup wouldn't catch it.
            let restore_uuid = uuid::Uuid::new_v4();
            let restore_db = atomic_cloud::tenant_db_name(restore_uuid);
            let base_url = std::env::var("ATOMIC_TEST_DATABASE_URL").unwrap();
            with_db_guard(&base_url, &restore_db, || async {
                restore_database(
                    &cluster,
                    &conn,
                    &restore_db,
                    &from_store,
                    atomic_cloud::DEFAULT_BACKUP_TIMEOUT,
                )
                .await
                .expect("restore into fresh db");

                // Open the restored database and assert the atom rehydrated.
                let restored_url = cluster.tenant_db_url(&restore_db).unwrap();
                let manager = DatabaseManager::new_postgres(".", &restored_url)
                    .await
                    .expect("open restored tenant");
                let core = manager.active_core().await.expect("restored core");
                let atom = core
                    .get_atom(&atom_id)
                    .await
                    .expect("query restored atom")
                    .expect("atom present after restore");
                assert!(
                    atom.atom.content.contains(MARKER),
                    "restored atom must carry the marker content: {:?}",
                    atom.atom.content
                );
                assert_eq!(atom.atom.source_url.as_deref(), Some(source_url));
                drop(core);
                drop(manager);
            })
            .await;
        },
    )
    .await;
}

#[tokio::test]
async fn restore_refuses_to_clobber_an_existing_database() {
    if !backup_tools_available().await {
        eprintln!(
            "restore_refuses_to_clobber_an_existing_database: skipping \
             (pg_dump/pg_restore not on PATH)"
        );
        return;
    }
    with_control_db(
        "restore_refuses_to_clobber_an_existing_database",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            let acct = provision_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                NewAccount {
                    email: "clobber@example.com".into(),
                    subdomain: "clobber".into(),
                },
            )
            .await
            .expect("provision");

            let conn = DumpConnection::for_cluster(&cluster).unwrap();
            let dump = dump_tenant_database(&conn, &acct.db_name, atomic_cloud::DEFAULT_BACKUP_TIMEOUT)
                .await
                .expect("dump");

            // Restoring onto the LIVE tenant database must be refused — a
            // restore that overwrote live data is the accident this guards.
            let err = restore_database(
                &cluster,
                &conn,
                &acct.db_name,
                &dump,
                atomic_cloud::DEFAULT_BACKUP_TIMEOUT,
            )
            .await
            .expect_err("restore must refuse an existing target");
            assert!(
                matches!(&err, atomic_cloud::CloudError::Backup(msg) if msg.contains("already exists")),
                "expected an 'already exists' Backup error, got {err:?}"
            );
        },
    )
    .await;
}

// ==================== Final dump before deletion ====================

#[tokio::test]
async fn delete_takes_final_dump_before_dropping() {
    if !backup_tools_available().await {
        eprintln!(
            "delete_takes_final_dump_before_dropping: skipping \
             (pg_dump/pg_restore not on PATH)"
        );
        return;
    }
    with_control_db(
        "delete_takes_final_dump_before_dropping",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            let acct = provision_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                NewAccount {
                    email: "final@example.com".into(),
                    subdomain: "finaldump".into(),
                },
            )
            .await
            .expect("provision");

            const MARKER: &str = "final-dump-marker-d4e8";
            let tenant_url = cluster.tenant_db_url(&acct.db_name).unwrap();
            {
                let manager = DatabaseManager::new_postgres(".", &tenant_url)
                    .await
                    .expect("open tenant");
                let core = manager.active_core().await.expect("active core");
                core.create_atom(
                    CreateAtomRequest {
                        content: format!("{MARKER} content"),
                        ..Default::default()
                    },
                    |_| {},
                )
                .await
                .expect("create atom");
                drop(core);
                drop(manager);
            }

            let (_dir, store) = temp_store();
            // delete_account with a backup store takes the final dump BEFORE the
            // drop (plan: "Account deletion" step 4).
            atomic_cloud::delete_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                // No billing provider in tests: the subscription-cancel step is
                // skipped (DEL-1 `billing` is `None`), exactly as the CLI/reaper paths.
                None,
                atomic_cloud::BackupPolicy::Required(&store),
                atomic_cloud::DeleteLock::Acquire,
                &acct.account_id,
                atomic_cloud::DEFAULT_BACKUP_TIMEOUT,
            )
            .await
            .expect("delete with final dump");

            // A final dump landed and captured real data — the marker is inside.
            let finals = store.list("backups/final/").await.unwrap();
            assert_eq!(finals.len(), 1, "exactly one final dump: {finals:?}");
            assert!(
                finals[0].contains(&acct.account_id),
                "final key names the account: {}",
                finals[0]
            );
            let bytes = store.get(&finals[0]).await.unwrap();
            assert_eq!(
                &bytes[..5],
                b"PGDMP",
                "final dump is a real custom-format dump"
            );
            // The dump is non-trivial (it captured a populated database, not an
            // empty schema) — a strong signal the data was dumped before the drop.
            assert!(
                bytes.len() > 1000,
                "final dump captured real data: {} bytes",
                bytes.len()
            );

            // The tenant database is now actually gone (the drop ran after the
            // dump), and a re-run is a no-op (idempotent), taking no second dump
            // because the database no longer exists.
            atomic_cloud::delete_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                // No billing provider in tests: the subscription-cancel step is
                // skipped (DEL-1 `billing` is `None`), exactly as the CLI/reaper paths.
                None,
                atomic_cloud::BackupPolicy::Required(&store),
                atomic_cloud::DeleteLock::Acquire,
                &acct.account_id,
                atomic_cloud::DEFAULT_BACKUP_TIMEOUT,
            )
            .await
            .expect("idempotent re-delete");
            let finals_after = store.list("backups/final/").await.unwrap();
            assert_eq!(
                finals_after.len(),
                1,
                "a retried deletion past the drop takes no second final dump: {finals_after:?}"
            );
        },
    )
    .await;
}

/// A [`BackupStore`] whose `put` always fails — used to prove the fail-closed
/// guarantee: a final-dump *upload* failure must abort `delete_account` before
/// any drop, leaving the tenant database and its control row intact and the
/// deletion retryable. `get`/`list`/`exists` are unused by this path.
struct FailingPutStore;

#[async_trait::async_trait]
impl BackupStore for FailingPutStore {
    async fn put(&self, _key: &str, _bytes: Vec<u8>) -> Result<(), atomic_cloud::CloudError> {
        Err(atomic_cloud::CloudError::BackupStore(
            "simulated upload failure".into(),
        ))
    }
    async fn get(&self, _key: &str) -> Result<Vec<u8>, atomic_cloud::CloudError> {
        unreachable!("the fail-closed delete path never reads")
    }
    async fn list(&self, _prefix: &str) -> Result<Vec<String>, atomic_cloud::CloudError> {
        unreachable!("the fail-closed delete path never lists")
    }
    async fn exists(&self, _key: &str) -> Result<bool, atomic_cloud::CloudError> {
        unreachable!("the fail-closed delete path never probes")
    }
}

/// The load-bearing fail-closed guarantee: when the final dump cannot be
/// stored, `delete_account` must error and drop nothing. With hard-delete v1
/// the final dump is the operator's only undo (plan: "Backups & disaster
/// recovery"), so a failed dump that nonetheless dropped the tenant would be
/// unrecoverable customer-data loss. Asserts the negative path the happy-path
/// test cannot: after a delete that *errors*, the tenant database still exists
/// and the `account_databases` row is intact (the account is fully retryable).
#[tokio::test]
async fn failed_final_dump_aborts_delete_before_dropping() {
    if !backup_tools_available().await {
        eprintln!(
            "failed_final_dump_aborts_delete_before_dropping: skipping \
             (pg_dump/pg_restore not on PATH)"
        );
        return;
    }
    with_control_db(
        "failed_final_dump_aborts_delete_before_dropping",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            let acct = provision_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                NewAccount {
                    email: "failclosed@example.com".into(),
                    subdomain: "failclosed".into(),
                },
            )
            .await
            .expect("provision");

            // Sanity: the tenant database and its mapping row exist pre-delete.
            assert!(
                atomic_cloud::tenant_database_exists(&cluster, &acct.db_name)
                    .await
                    .unwrap(),
                "tenant database exists before the delete"
            );

            // Delete with a store that fails the dump *upload*. The dump itself
            // runs (pg_dump succeeds), but the put fails — the error must
            // propagate before step 5's terminate_and_drop.
            let store: Arc<dyn BackupStore> = Arc::new(FailingPutStore);
            let err = atomic_cloud::delete_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                // No billing provider in tests: the subscription-cancel step is
                // skipped (DEL-1 `billing` is `None`), exactly as the CLI/reaper paths.
                None,
                atomic_cloud::BackupPolicy::Required(&store),
                atomic_cloud::DeleteLock::Acquire,
                &acct.account_id,
                atomic_cloud::DEFAULT_BACKUP_TIMEOUT,
            )
            .await
            .expect_err("a failed final dump must abort the delete");
            assert!(
                matches!(&err, atomic_cloud::CloudError::BackupStore(_)),
                "expected a BackupStore error from the failed upload, got {err:?}"
            );

            // The cardinal guarantee: NOTHING was dropped. The tenant database
            // is still present, so the operator can retry once the store
            // recovers — no customer data was lost.
            assert!(
                atomic_cloud::tenant_database_exists(&cluster, &acct.db_name)
                    .await
                    .unwrap(),
                "tenant database must still exist after a failed final dump"
            );

            // And the control row is intact (delete aborted before step 6's
            // mapping-row removal), so the account is fully retryable.
            let row_count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM account_databases WHERE account_id = $1")
                    .bind(&acct.account_id)
                    .fetch_one(control.pool())
                    .await
                    .unwrap();
            assert_eq!(row_count, 1, "the account_databases row must survive");

            // Clean up: a successful delete (local store) now drops the tenant
            // DB so the suite leaves nothing behind.
            let (_dir, ok_store) = temp_store();
            atomic_cloud::delete_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                // No billing provider in tests: the subscription-cancel step is
                // skipped (DEL-1 `billing` is `None`), exactly as the CLI/reaper paths.
                None,
                atomic_cloud::BackupPolicy::Required(&ok_store),
                atomic_cloud::DeleteLock::Acquire,
                &acct.account_id,
                atomic_cloud::DEFAULT_BACKUP_TIMEOUT,
            )
            .await
            .expect("cleanup delete succeeds once the store recovers");
            assert!(
                !atomic_cloud::tenant_database_exists(&cluster, &acct.db_name)
                    .await
                    .unwrap(),
                "tenant database is gone after the successful retry"
            );
        },
    )
    .await;
}

// ==================== db-name validation ====================

#[tokio::test]
async fn dump_and_restore_reject_bad_db_names() {
    // No cluster/tools needed: validation happens before any process spawn.
    let conn = DumpConnection::from_url("postgres://u:pw@h:5432/x").unwrap();
    let cluster = ClusterConfig {
        cluster_id: "c".into(),
        cluster_url: "postgres://u:pw@h:5432/x".into(),
    };
    for bad in [
        "not_a_tenant",
        "acct_short",
        "acct_\"; DROP DATABASE x; --",
        "default",
    ] {
        assert!(
            matches!(
                dump_tenant_database(&conn, bad, atomic_cloud::DEFAULT_BACKUP_TIMEOUT).await,
                Err(atomic_cloud::CloudError::InvalidDatabaseName(_))
            ),
            "dump must reject bad db name {bad:?}"
        );
        assert!(
            matches!(
                restore_database(
                    &cluster,
                    &conn,
                    bad,
                    b"ignored",
                    atomic_cloud::DEFAULT_BACKUP_TIMEOUT
                )
                .await,
                Err(atomic_cloud::CloudError::InvalidDatabaseName(_))
            ),
            "restore must reject bad db name {bad:?}"
        );
    }
}

// ==================== The rehearsed restore runbook (final-dump roundtrip) ====

/// The full disaster-recovery runbook, as a test (plan: "Restore runbook" —
/// "write and *rehearse* before launch"). It exercises the real operator
/// sequence end to end:
///
/// 1. provision a tenant and write a recognizable atom,
/// 2. `delete_account` with a configured store (takes the FINAL dump, then
///    drops the tenant DB — the account's data is now only in `backups/final/`),
/// 3. restore that exact final dump into a FRESH database,
/// 4. repoint `account_databases.db_name` to the restored database (the
///    runbook's manual step the CLI deliberately leaves to the operator),
/// 5. assert the atom is present in the restored tenant.
///
/// This is the proof that the final dump is a real, restorable undo — not just
/// bytes in a bucket — and that a per-tenant restore touches only that
/// account's row and its own new database.
#[tokio::test]
async fn final_dump_restore_runbook_roundtrip() {
    if !backup_tools_available().await {
        eprintln!(
            "final_dump_restore_runbook_roundtrip: skipping \
             (pg_dump/pg_restore not on PATH)"
        );
        return;
    }
    with_control_db("final_dump_restore_runbook_roundtrip", |url| async move {
        let (control, cluster) = setup(&url).await;
        let acct = provision_account(
            &control,
            &cluster,
            &ManagedKeys::Disabled,
            NewAccount {
                email: "runbook@example.com".into(),
                subdomain: "runbook".into(),
            },
        )
        .await
        .expect("provision");

        // 1 — write a recognizable atom into the live tenant.
        const MARKER: &str = "runbook-restore-marker-91bc";
        let source_url = "https://example.com/runbook-source";
        let tenant_url = cluster.tenant_db_url(&acct.db_name).unwrap();
        let atom_id = {
            let manager = DatabaseManager::new_postgres(".", &tenant_url)
                .await
                .expect("open tenant");
            let core = manager.active_core().await.expect("active core");
            let created = core
                .create_atom(
                    CreateAtomRequest {
                        content: format!("# Heading\n\n{MARKER} body"),
                        source_url: Some(source_url.to_string()),
                        ..Default::default()
                    },
                    |_| {},
                )
                .await
                .expect("create atom")
                .expect("atom inserted");
            drop(core);
            drop(manager);
            created.atom.id
        };

        // 2 — delete the account WITH a store: the final dump is taken before
        // the drop, then the live tenant DB is gone (its data now lives only
        // in backups/final/).
        let (_dir, store) = temp_store();
        delete_account(
            &control,
            &cluster,
            &ManagedKeys::Disabled,
            // No billing provider in tests: the subscription-cancel step is
            // skipped (DEL-1 `billing` is `None`), exactly as the CLI/reaper paths.
            None,
            atomic_cloud::BackupPolicy::Required(&store),
            atomic_cloud::DeleteLock::Acquire,
            &acct.account_id,
            atomic_cloud::DEFAULT_BACKUP_TIMEOUT,
        )
        .await
        .expect("delete takes the final dump then drops");
        assert!(
            !atomic_cloud::tenant_database_exists(&cluster, &acct.db_name)
                .await
                .unwrap(),
            "the original tenant database is gone after deletion"
        );
        let finals = store.list("backups/final/").await.unwrap();
        assert_eq!(finals.len(), 1, "exactly one final dump: {finals:?}");
        let final_key = finals[0].clone();

        // 3 — restore that final dump into a FRESH tenant database. (The
        // accounts row is hard-deleted by the delete, so we re-create a
        // mapping below to mimic an operator who reinstates the account; the
        // restore itself only needs the dump bytes + a fresh DB name.)
        let restore_uuid = uuid::Uuid::new_v4();
        let restore_db = tenant_db_name(restore_uuid);
        let conn = DumpConnection::for_cluster(&cluster).unwrap();
        let dump_bytes = store.get(&final_key).await.expect("read final dump");
        let base_url = std::env::var("ATOMIC_TEST_DATABASE_URL").unwrap();
        with_db_guard(&base_url, &restore_db, || async {
            restore_database(
                &cluster,
                &conn,
                &restore_db,
                &dump_bytes,
                atomic_cloud::DEFAULT_BACKUP_TIMEOUT,
            )
            .await
            .expect("restore the final dump into a fresh db");

            // 4 — repoint the account's mapping to the restored database (the
            // runbook's manual control-plane step). We re-seed the account +
            // mapping for the restored UUID so the repointed row is well-formed
            // and isolated to this tenant.
            sqlx::query(
                "INSERT INTO accounts (id, subdomain, email, status, plan, created_at) \
                 VALUES ($1, 'runbook-restored', 'runbook@example.com', 'active', 'free', NOW())",
            )
            .bind(restore_uuid.to_string())
            .execute(control.pool())
            .await
            .expect("reinstate account row");
            sqlx::query(
                "INSERT INTO account_databases (account_id, cluster_id, db_name, status) \
                 VALUES ($1, 'test-cluster-1', $2, 'active')",
            )
            .bind(restore_uuid.to_string())
            .bind(&restore_db)
            .execute(control.pool())
            .await
            .expect("repoint mapping to the restored database");

            // The repoint touched only this account's row — no other mapping
            // exists for the original account (it was hard-deleted).
            let orphan_rows: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM account_databases WHERE account_id = $1")
                    .bind(&acct.account_id)
                    .fetch_one(control.pool())
                    .await
                    .unwrap();
            assert_eq!(orphan_rows, 0, "the deleted account keeps no mapping row");

            // 5 — the atom is present in the restored tenant.
            let restored_url = cluster.tenant_db_url(&restore_db).unwrap();
            let manager = DatabaseManager::new_postgres(".", &restored_url)
                .await
                .expect("open restored tenant");
            let core = manager.active_core().await.expect("restored core");
            let atom = core
                .get_atom(&atom_id)
                .await
                .expect("query restored atom")
                .expect("atom present after restore from the final dump");
            assert!(
                atom.atom.content.contains(MARKER),
                "restored atom carries the marker: {:?}",
                atom.atom.content
            );
            assert_eq!(atom.atom.source_url.as_deref(), Some(source_url));
            drop(core);
            drop(manager);
        })
        .await;
    })
    .await;
}

// ==================== backup status / list helper queries ====================

/// `backup status` reads: per-tenant `last_backup_at` (+ last error), the
/// stale-tenant set, and the recent `backup_runs` ledger. Drives the same
/// queries the CLI's `Status` arm prints.
#[tokio::test]
async fn backup_status_reports_freshness_and_stale_tenants() {
    with_control_db(
        "backup_status_reports_freshness_and_stale_tenants",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            let fresh = provision_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                NewAccount {
                    email: "fresh@example.com".into(),
                    subdomain: "freshtenant".into(),
                },
            )
            .await
            .expect("provision fresh");
            let stale = provision_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                NewAccount {
                    email: "stale@example.com".into(),
                    subdomain: "staletenant".into(),
                },
            )
            .await
            .expect("provision stale");

            // Backdate both accounts past the staleness horizon so a
            // never-/long-ago-backed-up tenant actually trips the alert.
            sqlx::query("UPDATE accounts SET created_at = NOW() - INTERVAL '3 days'")
                .execute(control.pool())
                .await
                .unwrap();

            let now = chrono::Utc::now();
            // Only `fresh` gets a recent successful backup.
            record_backup_success(&control, &fresh.account_id, &fresh.db_name, now)
                .await
                .unwrap();

            // Per-tenant status: both listed, fresh stamped, stale never.
            let statuses = tenant_backup_status(&control).await.unwrap();
            assert_eq!(statuses.len(), 2, "both active tenants listed");
            let fresh_row = statuses
                .iter()
                .find(|s| s.account_id == fresh.account_id)
                .expect("fresh listed");
            assert!(fresh_row.last_backup_at.is_some(), "fresh is stamped");
            assert_eq!(fresh_row.subdomain, "freshtenant");
            let stale_row = statuses
                .iter()
                .find(|s| s.account_id == stale.account_id)
                .expect("stale listed");
            assert!(stale_row.last_backup_at.is_none(), "stale never backed up");

            // Staleness query at the 36h horizon: only the never-backed-up
            // tenant trips it.
            let stale_set =
                stale_tenant_backups(&control, std::time::Duration::from_secs(36 * 60 * 60))
                    .await
                    .unwrap();
            assert_eq!(
                stale_set.len(),
                1,
                "exactly one stale tenant: {stale_set:?}"
            );
            assert_eq!(stale_set[0].account_id, stale.account_id);

            // A failure on the fresh tenant surfaces its error but keeps it
            // out of the stale set (its last success is still recent).
            record_backup_failure(
                &control,
                &fresh.account_id,
                &fresh.db_name,
                "pg_dump: nope",
                chrono::Utc::now(),
            )
            .await
            .unwrap();
            let statuses = tenant_backup_status(&control).await.unwrap();
            let fresh_row = statuses
                .iter()
                .find(|s| s.account_id == fresh.account_id)
                .unwrap();
            assert_eq!(
                fresh_row.last_backup_error.as_deref(),
                Some("pg_dump: nope")
            );
            assert!(
                fresh_row.last_backup_at.is_some(),
                "failure keeps last success"
            );

            // Recent runs ledger: a couple of rows come back newest-first.
            let r1 = start_backup_run(&control, "nightly").await.unwrap();
            atomic_cloud::finish_backup_run(&control, &r1, 2, 1, 1)
                .await
                .unwrap();
            let r2 = start_backup_run(&control, "final").await.unwrap();
            let runs = recent_backup_runs(&control, 10).await.unwrap();
            assert!(runs.len() >= 2, "at least the two runs we inserted");
            assert_eq!(runs[0].id, r2, "newest first");
            assert_eq!(runs[0].kind, "final");
            assert!(runs[0].finished_at.is_none(), "r2 still in-flight");
            let r1_row = runs.iter().find(|r| r.id == r1).expect("r1 present");
            assert_eq!(
                (r1_row.total, r1_row.succeeded, r1_row.failed),
                (Some(2), Some(1), Some(1))
            );
        },
    )
    .await;
}

/// `backup list --subdomain` reads: a tenant's own dumps (nightly by db_name,
/// final by account id) and never another tenant's. Drives the same
/// `dumps_for_account` query the CLI's `List` arm prints.
#[tokio::test]
async fn dumps_for_account_lists_only_that_tenants_dumps() {
    let (_dir, store) = temp_store();
    let acct_a = "11111111-2222-3333-4444-555555555555";
    let db_a = "acct_aaaaaaaaaaaaaaaaaaaaaaaaaa";
    let acct_b = "99999999-8888-7777-6666-555555555555";
    let db_b = "acct_bbbbbbbbbbbbbbbbbbbbbbbbbb";

    // Two nightly dumps (different days) + one final for tenant A.
    store
        .put(
            "backups/2026-06-10/acct_aaaaaaaaaaaaaaaaaaaaaaaaaa.dump",
            b"a1".to_vec(),
        )
        .await
        .unwrap();
    store
        .put(
            "backups/2026-06-11/acct_aaaaaaaaaaaaaaaaaaaaaaaaaa.dump",
            b"a2".to_vec(),
        )
        .await
        .unwrap();
    store
        .put(
            &format!("backups/final/{acct_a}-20260612T010203Z.dump"),
            b"af".to_vec(),
        )
        .await
        .unwrap();
    // Tenant B's dumps + a control-plane dump must NOT leak into A's listing.
    store
        .put(
            "backups/2026-06-11/acct_bbbbbbbbbbbbbbbbbbbbbbbbbb.dump",
            b"b1".to_vec(),
        )
        .await
        .unwrap();
    store
        .put(
            &format!("backups/final/{acct_b}-20260612T010203Z.dump"),
            b"bf".to_vec(),
        )
        .await
        .unwrap();
    store
        .put("backups/2026-06-11/control.dump", b"ctl".to_vec())
        .await
        .unwrap();

    let keys = dumps_for_account(&store, acct_a, db_a).await.unwrap();
    assert_eq!(
        keys,
        vec![
            "backups/2026-06-10/acct_aaaaaaaaaaaaaaaaaaaaaaaaaa.dump".to_string(),
            "backups/2026-06-11/acct_aaaaaaaaaaaaaaaaaaaaaaaaaa.dump".to_string(),
            format!("backups/final/{acct_a}-20260612T010203Z.dump"),
        ],
        "tenant A sees exactly its two nightly dumps + its final, sorted"
    );

    // Tenant B is symmetric and disjoint — no cross-tenant leakage.
    let keys_b = dumps_for_account(&store, acct_b, db_b).await.unwrap();
    assert_eq!(
        keys_b,
        vec![
            "backups/2026-06-11/acct_bbbbbbbbbbbbbbbbbbbbbbbbbb.dump".to_string(),
            format!("backups/final/{acct_b}-20260612T010203Z.dump"),
        ]
    );
}
