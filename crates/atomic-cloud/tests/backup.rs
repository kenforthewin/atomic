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
    dump_tenant_database, list_active_tenant_databases, provision_account, record_backup_failure,
    record_backup_success, restore_database, stale_tenant_backups, start_backup_run, BackupStore,
    ClusterConfig, ControlPlane, DumpConnection, LocalFileSystemStore, ManagedKeys, NewAccount,
};
use atomic_core::{CreateAtomRequest, DatabaseManager};
use support::{with_control_db, with_db_guard};

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
        let stale = stale_tenant_backups(&control, std::time::Duration::ZERO, now)
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
        let fresh =
            stale_tenant_backups(&control, std::time::Duration::from_secs(36 * 60 * 60), now)
                .await
                .unwrap();
        assert!(fresh.is_empty(), "a just-backed-up tenant is not stale");

        // A failure records the error but does NOT reset last_backup_at — the
        // monitor must keep seeing the last *success*, so a tenant whose
        // backups start failing still trips the alert by its stale success.
        record_backup_failure(&control, &acct.account_id, &acct.db_name, "pg_dump: boom")
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
            let dump = dump_tenant_database(&conn, &acct.db_name)
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
                restore_database(&cluster, &conn, &restore_db, &from_store)
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
            let dump = dump_tenant_database(&conn, &acct.db_name)
                .await
                .expect("dump");

            // Restoring onto the LIVE tenant database must be refused — a
            // restore that overwrote live data is the accident this guards.
            let err = restore_database(&cluster, &conn, &acct.db_name, &dump)
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
                Some(&store),
                &acct.account_id,
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
                Some(&store),
                &acct.account_id,
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
                Some(&store),
                &acct.account_id,
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
                Some(&ok_store),
                &acct.account_id,
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
                dump_tenant_database(&conn, bad).await,
                Err(atomic_cloud::CloudError::InvalidDatabaseName(_))
            ),
            "dump must reject bad db name {bad:?}"
        );
        assert!(
            matches!(
                restore_database(&cluster, &conn, bad, b"ignored").await,
                Err(atomic_cloud::CloudError::InvalidDatabaseName(_))
            ),
            "restore must reject bad db name {bad:?}"
        );
    }
}
