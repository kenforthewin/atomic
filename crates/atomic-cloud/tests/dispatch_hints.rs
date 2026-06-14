//! Dispatch-hint lifecycle integration tests (plan: "Worker fairness & job
//! queue" → "Cross-tenant ledger scan").
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command. The HTTP side of the lifecycle — a
//! mutating tenant request marking the hint — is pinned in
//! `tests/e2e_cloud.rs`, where the composed server harness lives.

mod support;

use atomic_cloud::{
    clear_hint_if_older, delete_account, list_active_account_ids, list_hinted_accounts, mark_hint,
    provision_account, ControlPlane, ManagedKeys, NewAccount,
};
use support::with_control_db;

/// Migrated control plane handle.
async fn setup(control_url: &str) -> ControlPlane {
    let control = ControlPlane::connect(control_url)
        .await
        .expect("connect control plane");
    control.initialize().await.expect("migrate control plane");
    control
}

/// Insert a bare accounts row — hint CRUD needs the FK target, not a real
/// tenant database.
async fn insert_account(control: &ControlPlane, id: &str, subdomain: &str, status: &str) {
    sqlx::query(
        "INSERT INTO accounts (id, subdomain, email, status, plan) \
         VALUES ($1, $2, $3, $4, 'free')",
    )
    .bind(id)
    .bind(subdomain)
    .bind(format!("{subdomain}@example.com"))
    .bind(status)
    .execute(control.pool())
    .await
    .expect("insert account row");
}

async fn hinted_ids(control: &ControlPlane) -> Vec<String> {
    list_hinted_accounts(control)
        .await
        .expect("list hints")
        .into_iter()
        .map(|h| h.account_id)
        .collect()
}

#[tokio::test]
async fn hint_crud_marks_bumps_and_clears() {
    with_control_db("hint_crud_marks_bumps_and_clears", |url| async move {
        let control = setup(&url).await;
        insert_account(&control, "acct-1", "alpha", "active").await;
        insert_account(&control, "acct-2", "bravo", "active").await;

        assert!(hinted_ids(&control).await.is_empty(), "no hints initially");

        // Mark both; re-marking is an UPSERT, not a duplicate row.
        mark_hint(&control, "acct-1").await.expect("mark acct-1");
        mark_hint(&control, "acct-2").await.expect("mark acct-2");
        mark_hint(&control, "acct-1").await.expect("re-mark acct-1");
        let hints = list_hinted_accounts(&control).await.expect("list hints");
        assert_eq!(hints.len(), 2, "one row per account: {hints:?}");
        // Oldest stamp first: acct-1's re-mark pushed it behind acct-2.
        assert_eq!(
            hints
                .iter()
                .map(|h| h.account_id.as_str())
                .collect::<Vec<_>>(),
            vec!["acct-2", "acct-1"],
            "hints list oldest stamp first"
        );

        // An up-to-date clear removes the row.
        let stamp = hints
            .iter()
            .find(|h| h.account_id == "acct-1")
            .expect("acct-1 hinted")
            .last_enqueued_at;
        assert!(
            clear_hint_if_older(&control, "acct-1", stamp)
                .await
                .expect("clear acct-1"),
            "clear with the current stamp must remove the hint"
        );
        assert_eq!(hinted_ids(&control).await, vec!["acct-2".to_string()]);

        // Clearing an absent hint reports false.
        assert!(
            !clear_hint_if_older(&control, "acct-1", stamp)
                .await
                .expect("re-clear acct-1"),
            "clearing an already-cleared hint is a no-op"
        );
    })
    .await;
}

/// The dual-write loss bound (plan: "Cross-tenant ledger scan"): a hint
/// bumped DURING a scan must survive the scan's clear. The scan reads the
/// stamp at start; only a clear against a stamp at least as new as the
/// row's may delete it.
#[tokio::test]
async fn hint_bumped_mid_scan_survives_clear() {
    with_control_db("hint_bumped_mid_scan_survives_clear", |url| async move {
        let control = setup(&url).await;
        insert_account(&control, "acct-1", "alpha", "active").await;

        // Scan start: read the hint and remember its stamp.
        mark_hint(&control, "acct-1").await.expect("mark");
        let scan_read = list_hinted_accounts(&control).await.expect("list")[0].last_enqueued_at;

        // Mid-scan: a tenant request enqueues more work and bumps the hint.
        // The pause guarantees a strictly newer NOW() stamp.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        mark_hint(&control, "acct-1").await.expect("bump mid-scan");

        // Scan end ("ledger came back empty"): the clear carries the stamp
        // read at scan start and must NOT remove the bumped hint.
        assert!(
            !clear_hint_if_older(&control, "acct-1", scan_read)
                .await
                .expect("conditional clear"),
            "a hint bumped mid-scan must survive the scan's clear"
        );
        assert_eq!(
            hinted_ids(&control).await,
            vec!["acct-1".to_string()],
            "the bumped hint is still listed for the next pass"
        );

        // The next pass reads the new stamp; its clear succeeds.
        let next_read = list_hinted_accounts(&control).await.expect("list")[0].last_enqueued_at;
        assert!(
            clear_hint_if_older(&control, "acct-1", next_read)
                .await
                .expect("next-pass clear"),
            "the next pass clears against the fresh stamp"
        );
        assert!(hinted_ids(&control).await.is_empty());
    })
    .await;
}

/// Marking a hint for an account that no longer exists (deleted between the
/// caller's auth and the write) is success, not an FK error — a hint for a
/// dead account is meaningless.
#[tokio::test]
async fn mark_hint_for_missing_account_is_quietly_dropped() {
    with_control_db(
        "mark_hint_for_missing_account_is_quietly_dropped",
        |url| async move {
            let control = setup(&url).await;
            mark_hint(&control, "no-such-account")
                .await
                .expect("FK violation must be swallowed");
            assert!(hinted_ids(&control).await.is_empty(), "nothing stored");
        },
    )
    .await;
}

/// The slow-path full scan's input: every `active` account, regardless of
/// hints; non-active accounts (provisioning, mid-deletion) are excluded.
#[tokio::test]
async fn slow_path_scan_lists_all_active_accounts() {
    with_control_db(
        "slow_path_scan_lists_all_active_accounts",
        |url| async move {
            let control = setup(&url).await;
            insert_account(&control, "acct-1", "alpha", "active").await;
            insert_account(&control, "acct-2", "bravo", "active").await;
            insert_account(&control, "acct-3", "charlie", "provisioning").await;

            // Only acct-2 is hinted — the full scan must not care.
            mark_hint(&control, "acct-2").await.expect("mark acct-2");

            let active = list_active_account_ids(&control)
                .await
                .expect("list active accounts");
            assert_eq!(
                active,
                vec!["acct-1".to_string(), "acct-2".to_string()],
                "all active accounts regardless of hints; non-active excluded"
            );
        },
    )
    .await;
}

/// Account deletion sweeps the hint row via the FK CASCADE — the deletion
/// sequence deletes the accounts row, and the hint dies with it (no
/// explicit step, same safety-net pattern as every account-owned table).
#[tokio::test]
async fn hint_rows_cascade_with_account_deletion() {
    with_control_db(
        "hint_rows_cascade_with_account_deletion",
        |url| async move {
            let control = setup(&url).await;
            let cluster = atomic_cloud::ClusterConfig {
                cluster_id: "test-cluster-1".to_string(),
                cluster_url: std::env::var("ATOMIC_TEST_DATABASE_URL")
                    .expect("with_control_db verified ATOMIC_TEST_DATABASE_URL"),
            };

            let account = provision_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                NewAccount {
                    email: "kenny@example.com".to_string(),
                    subdomain: "kenny".to_string(),
                },
            )
            .await
            .expect("provision account");

            mark_hint(&control, &account.account_id)
                .await
                .expect("mark provisioned account");
            assert_eq!(hinted_ids(&control).await, vec![account.account_id.clone()]);

            delete_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                None,
                &account.account_id,
            )
            .await
            .expect("delete account");

            assert!(
                hinted_ids(&control).await.is_empty(),
                "the hint row must CASCADE away with the accounts row"
            );
        },
    )
    .await;
}
