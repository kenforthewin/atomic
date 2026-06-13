//! Managed-key lifecycle integration tests (plan: "Provider management" →
//! "Managed key lifecycle"; signup step 9; deletion step 3; the rollback
//! paths).
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command. Every test drives the production
//! functions (`provision_account`, `delete_account`, `run_reaper_pass`)
//! against a [`RecordingProvisioning`] — NO REAL PROVIDERS — and asserts on
//! both the recorded API traffic and the control-plane state, including
//! that key plaintext is never at rest.

mod support;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use atomic_cloud::reaper::{run_reaper_pass, ReaperPolicy};
use atomic_cloud::{
    delete_account, get_credentials, provision_account, tenant_db_name, CloudError, ClusterConfig,
    ControlPlane, CredentialOrigin, EnvMasterKeyVault, ManagedKeys, NewAccount, NewCredentials,
    Provider, SecretKey,
};
use support::{
    control_db_contains, managed_keys_with, with_control_db, ProvisioningCall,
    RecordingProvisioning, TEST_MASTER_KEY,
};
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

fn test_vault() -> EnvMasterKeyVault {
    EnvMasterKeyVault::new(TEST_MASTER_KEY)
}

async fn account_status(control: &ControlPlane, account_id: &str) -> Option<String> {
    sqlx::query_scalar("SELECT status FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_optional(control.pool())
        .await
        .expect("read account status")
}

async fn active_provider(control: &ControlPlane, account_id: &str) -> Option<(String, String)> {
    sqlx::query_as(
        "SELECT active_provider, active_origin FROM accounts \
         WHERE id = $1 AND active_provider IS NOT NULL",
    )
    .bind(account_id)
    .fetch_optional(control.pool())
    .await
    .expect("read active provider pointer")
}

async fn credentials_count(control: &ControlPlane, account_id: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM provider_credentials WHERE account_id = $1")
        .bind(account_id)
        .fetch_one(control.pool())
        .await
        .expect("count credentials rows")
}

/// Signup step 9: exactly one key minted, with the plan's allowance and
/// monthly reset; the encrypted row stores its id and decrypts back to the
/// minted plaintext; the active-provider pointer lands on it; the plaintext
/// is never at rest.
#[tokio::test]
async fn signup_creates_exactly_one_key_and_encrypted_row() {
    with_control_db(
        "signup_creates_exactly_one_key_and_encrypted_row",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            let api = Arc::new(RecordingProvisioning::default());
            let managed = managed_keys_with(Arc::clone(&api));

            let account = provision_account(
                &control,
                &cluster,
                &managed,
                new_account("k@example.com", "keyed"),
            )
            .await
            .expect("provision");

            // Exactly one create, with the configured name/limit/reset args.
            let (plaintext, external_key_id) = RecordingProvisioning::nth_key(0);
            assert_eq!(
                api.calls(),
                vec![ProvisioningCall::Create {
                    name: format!("atomic-cloud/{}", account.account_id),
                    credit_limit_cents: 50,
                    monthly_reset: true,
                }],
                "one create call, nothing else"
            );

            // The encrypted row: managed origin, the minted id, the minted
            // plaintext after decryption, the pinned model config.
            let creds = get_credentials(
                &control,
                &test_vault(),
                &account.account_id,
                Provider::OpenRouter,
                CredentialOrigin::Managed,
            )
            .await
            .expect("fetch credentials")
            .expect("managed row exists");
            assert_eq!(creds.external_key_id.as_deref(), Some(&*external_key_id));
            assert_eq!(creds.api_key.expose(), plaintext);
            assert_eq!(
                creds.model_config["embedding_model"],
                serde_json::json!("openai/text-embedding-3-small")
            );
            assert_eq!(credentials_count(&control, &account.account_id).await, 1);

            // The account's active provider is the managed row.
            assert_eq!(
                active_provider(&control, &account.account_id).await,
                Some(("openrouter".to_string(), "managed".to_string()))
            );
            assert_eq!(
                account_status(&control, &account.account_id)
                    .await
                    .as_deref(),
                Some("active")
            );

            // SECRET HYGIENE: the plaintext appears in no text column of
            // any control-plane table.
            assert!(
                !control_db_contains(&url, &plaintext).await,
                "runtime-key plaintext found at rest"
            );
        },
    )
    .await;
}

/// Crash-resume idempotency: a provision that died *after* the credentials
/// row was written must not mint a second key when resumed — the row is the
/// record that a key was already paid for. The resume also heals a missing
/// active-provider pointer (a crash between the row insert and the flip).
#[tokio::test]
async fn crash_resume_after_row_exists_creates_no_second_key() {
    with_control_db(
        "crash_resume_after_row_exists_creates_no_second_key",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            let account_id = Uuid::new_v4();

            // The crash state: claimed account, tenant database created and
            // migrated... none of that matters to step 9 — what matters is
            // the credentials row already exists (with NO active pointer:
            // the crash landed between insert and flip).
            sqlx::query(
                "INSERT INTO accounts (id, subdomain, email, status, plan) \
                 VALUES ($1, 'resumed', 'k@example.com', 'provisioning', 'free')",
            )
            .bind(account_id.to_string())
            .execute(control.pool())
            .await
            .expect("seed provisioning row");
            atomic_cloud::upsert_credentials(
                &control,
                &test_vault(),
                &account_id.to_string(),
                NewCredentials {
                    provider: Provider::OpenRouter,
                    origin: CredentialOrigin::Managed,
                    api_key: SecretKey::new("sk-or-v1-from-first-run".to_string()),
                    external_key_id: Some("orkey-from-first-run".to_string()),
                    model_config: serde_json::json!({}),
                },
            )
            .await
            .expect("seed credentials row");

            let api = Arc::new(RecordingProvisioning::default());
            let managed = managed_keys_with(Arc::clone(&api));
            let account = provision_account(
                &control,
                &cluster,
                &managed,
                new_account("k@example.com", "resumed"),
            )
            .await
            .expect("resume completes");
            assert_eq!(account.account_id, account_id.to_string());

            // No second key: zero API calls of any kind.
            assert_eq!(api.calls(), vec![], "resume must not touch the API");

            // The first run's key survives, and the pointer was healed.
            let creds = get_credentials(
                &control,
                &test_vault(),
                &account.account_id,
                Provider::OpenRouter,
                CredentialOrigin::Managed,
            )
            .await
            .expect("fetch")
            .expect("row survives the resume");
            assert_eq!(
                creds.external_key_id.as_deref(),
                Some("orkey-from-first-run")
            );
            assert_eq!(credentials_count(&control, &account.account_id).await, 1);
            assert_eq!(
                active_provider(&control, &account.account_id).await,
                Some(("openrouter".to_string(), "managed".to_string())),
                "resume heals the missing active pointer"
            );
        },
    )
    .await;
}

/// Provisioning-mode 'disabled': the account provisions to active with zero
/// provisioning-API involvement and no credentials row — the keyless dev
/// shape. (`ManagedKeys::Disabled` holds no API client at all, so "zero API
/// calls" is enforced by construction; the row/pointer absence is what's
/// asserted.)
#[tokio::test]
async fn disabled_mode_provisions_keyless_accounts() {
    with_control_db(
        "disabled_mode_provisions_keyless_accounts",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            let account = provision_account(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                new_account("k@example.com", "keyless"),
            )
            .await
            .expect("provision");

            assert_eq!(
                account_status(&control, &account.account_id)
                    .await
                    .as_deref(),
                Some("active")
            );
            assert_eq!(credentials_count(&control, &account.account_id).await, 0);
            assert_eq!(active_provider(&control, &account.account_id).await, None);
        },
    )
    .await;
}

/// `create_key` failure at signup: the provision fails with the typed
/// provisioning error, nothing half-exists (no credentials row, no mapping
/// row, account still 'provisioning'), and the account is reapable — a
/// later pass with a healthy provider resumes it to active with exactly one
/// key.
#[tokio::test]
async fn create_key_failure_is_typed_and_reapable() {
    with_control_db(
        "create_key_failure_is_typed_and_reapable",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            let api = Arc::new(RecordingProvisioning::default());
            api.fail_create.store(true, Ordering::SeqCst);
            let managed = managed_keys_with(Arc::clone(&api));

            let err = provision_account(
                &control,
                &cluster,
                &managed,
                new_account("k@example.com", "blocked"),
            )
            .await
            .expect_err("provision must fail when key creation fails");
            assert!(
                matches!(err, CloudError::ProviderProvisioning { .. }),
                "typed provisioning error, got: {err:?}"
            );

            // No half state: the claim survives as a reapable 'provisioning'
            // row; step 9 wrote nothing; steps 10-11 never ran.
            let account_id: String =
                sqlx::query_scalar("SELECT id FROM accounts WHERE subdomain = 'blocked'")
                    .fetch_one(control.pool())
                    .await
                    .expect("claim row exists");
            assert_eq!(
                account_status(&control, &account_id).await.as_deref(),
                Some("provisioning"),
                "account stays reapable"
            );
            assert_eq!(credentials_count(&control, &account_id).await, 0);
            let mappings: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM account_databases WHERE account_id = $1")
                    .bind(&account_id)
                    .fetch_one(control.pool())
                    .await
                    .expect("count mappings");
            assert_eq!(
                mappings, 0,
                "step 9 failure must precede the mapping insert"
            );

            // Recovery: the provider heals, the reaper resumes (the row is
            // young, so resume via the user's own retry path — same function).
            api.fail_create.store(false, Ordering::SeqCst);
            provision_account(
                &control,
                &cluster,
                &managed,
                new_account("k@example.com", "blocked"),
            )
            .await
            .expect("retry resumes the stuck provision");
            assert_eq!(
                account_status(&control, &account_id).await.as_deref(),
                Some("active")
            );
            assert_eq!(credentials_count(&control, &account_id).await, 1);
            // Two create attempts total: the failed one and the successful one.
            assert_eq!(api.creates().len(), 2);
        },
    )
    .await;
}

/// Reaper rollback after key creation: a stuck provision that got as far as
/// minting its key (credentials row written) and whose resume cannot
/// succeed is rolled back — and the rollback deletes the external key,
/// reading its id before the accounts-row CASCADE sweeps the row.
#[tokio::test]
async fn rollback_after_key_creation_deletes_the_external_key() {
    with_control_db(
        "rollback_after_key_creation_deletes_the_external_key",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            let account_id = Uuid::new_v4();

            // The crash state: a stale claim whose email cannot revalidate
            // (so the reaper's resume deterministically fails), with the
            // step-9 key already minted and recorded.
            sqlx::query(
                "INSERT INTO accounts (id, subdomain, email, status, plan, created_at) \
                 VALUES ($1, 'doomed-key', 'not-an-email', 'provisioning', 'free', \
                         NOW() - INTERVAL '10 minutes')",
            )
            .bind(account_id.to_string())
            .execute(control.pool())
            .await
            .expect("seed stale claim");
            atomic_cloud::upsert_credentials(
                &control,
                &test_vault(),
                &account_id.to_string(),
                NewCredentials {
                    provider: Provider::OpenRouter,
                    origin: CredentialOrigin::Managed,
                    api_key: SecretKey::new("sk-or-v1-doomed".to_string()),
                    external_key_id: Some("orkey-doomed".to_string()),
                    model_config: serde_json::json!({}),
                },
            )
            .await
            .expect("seed credentials row");

            let api = Arc::new(RecordingProvisioning::default());
            let managed = managed_keys_with(Arc::clone(&api));
            let summary =
                run_reaper_pass(&control, &cluster, &managed, &ReaperPolicy::default()).await;
            assert_eq!(summary.stuck_rolled_back, vec![account_id.to_string()]);
            assert!(
                summary.errors.is_empty(),
                "pass must not record errors: {:?}",
                summary.errors
            );

            // The external key was deleted with the right id, and the rows
            // are gone with the account.
            assert_eq!(api.deleted_key_ids(), vec!["orkey-doomed".to_string()]);
            assert_eq!(
                account_status(&control, &account_id.to_string()).await,
                None
            );
            assert_eq!(
                credentials_count(&control, &account_id.to_string()).await,
                0
            );
        },
    )
    .await;
}

/// Deletion step 3: `delete_account` deletes the managed key via the
/// provisioning API before destroying the rows that reference it.
#[tokio::test]
async fn delete_account_deletes_the_managed_key() {
    with_control_db("delete_account_deletes_the_managed_key", |url| async move {
        let (control, cluster) = setup(&url).await;
        let api = Arc::new(RecordingProvisioning::default());
        let managed = managed_keys_with(Arc::clone(&api));

        let account = provision_account(
            &control,
            &cluster,
            &managed,
            new_account("k@example.com", "shortlived"),
        )
        .await
        .expect("provision");
        let (_, external_key_id) = RecordingProvisioning::nth_key(0);

        delete_account(&control, &cluster, &managed, &account.account_id)
            .await
            .expect("delete");

        assert_eq!(api.deleted_key_ids(), vec![external_key_id]);
        assert_eq!(account_status(&control, &account.account_id).await, None);
        assert_eq!(credentials_count(&control, &account.account_id).await, 0);

        // Best-effort contract: a deletion retry after the rows are gone
        // succeeds quietly and calls the API zero further times.
        delete_account(&control, &cluster, &managed, &account.account_id)
            .await
            .expect("retried delete is a no-op");
        assert_eq!(api.deleted_key_ids().len(), 1, "no second delete call");
    })
    .await;
}

/// A provider outage during deletion step 3 must not wedge the deletion:
/// the key delete fails (loudly logged), and the rest of the sequence still
/// completes. The residue is the documented accepted orphan.
#[tokio::test]
async fn deletion_proceeds_when_key_delete_fails() {
    with_control_db(
        "deletion_proceeds_when_key_delete_fails",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            let api = Arc::new(RecordingProvisioning::default());
            let managed = managed_keys_with(Arc::clone(&api));

            let account = provision_account(
                &control,
                &cluster,
                &managed,
                new_account("k@example.com", "outage"),
            )
            .await
            .expect("provision");

            api.fail_delete.store(true, Ordering::SeqCst);
            delete_account(&control, &cluster, &managed, &account.account_id)
                .await
                .expect("deletion must not wedge on a provider outage");

            // The delete was attempted, the account is fully gone regardless.
            assert_eq!(api.deleted_key_ids().len(), 1);
            assert_eq!(account_status(&control, &account.account_id).await, None);
        },
    )
    .await;
}

/// Interrupted-deletion recovery (reaper arm 3) re-runs `delete_account`,
/// whose step 3 finds the surviving credentials row and deletes the key —
/// covering an original attempt whose own key delete failed (the row only
/// dies with the accounts-row CASCADE, so the retry can still find the id).
#[tokio::test]
async fn interrupted_deletion_recovery_deletes_the_key() {
    with_control_db(
        "interrupted_deletion_recovery_deletes_the_key",
        |url| async move {
            let (control, cluster) = setup(&url).await;
            let api = Arc::new(RecordingProvisioning::default());
            let managed = managed_keys_with(Arc::clone(&api));

            let account = provision_account(
                &control,
                &cluster,
                &managed,
                new_account("k@example.com", "halfgone"),
            )
            .await
            .expect("provision");
            let (_, external_key_id) = RecordingProvisioning::nth_key(0);

            // Manufacture the interrupted-deletion state: active account,
            // no mapping row (deletion died between its steps 6 and 7 —
            // e.g. after a failed best-effort key delete), old enough for
            // the recovery grace.
            sqlx::query("DELETE FROM account_databases WHERE account_id = $1")
                .bind(&account.account_id)
                .execute(control.pool())
                .await
                .expect("remove mapping row");
            sqlx::query(
                "UPDATE accounts SET created_at = NOW() - INTERVAL '10 minutes' WHERE id = $1",
            )
            .bind(&account.account_id)
            .execute(control.pool())
            .await
            .expect("age the account");

            let summary =
                run_reaper_pass(&control, &cluster, &managed, &ReaperPolicy::default()).await;
            assert_eq!(
                summary.deletions_completed,
                vec![account.account_id.clone()]
            );
            assert!(
                summary.errors.is_empty(),
                "pass must not record errors: {:?}",
                summary.errors
            );

            assert_eq!(api.deleted_key_ids(), vec![external_key_id]);
            assert_eq!(account_status(&control, &account.account_id).await, None);
            // The deletion re-parked the subdomain, as always.
            let reserved: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM subdomains_reserved WHERE subdomain = 'halfgone')",
            )
            .fetch_one(control.pool())
            .await
            .expect("check reservation");
            assert!(reserved, "recovery completes the full deletion sequence");
        },
    )
    .await;
}

/// The lost-race cleanup inside step 9 itself: when the accounts row
/// vanishes between `create_key` and the credentials insert (a concurrent
/// deletion won — the narrowest orphan window), the insert hits the FK and
/// the just-minted key is deleted with the locally held id before the error
/// surfaces. Nothing references the key afterwards. (The sibling windows —
/// the mapping-insert 23503 and the zero-row activation — reuse the same
/// `delete_external_key_best_effort` cleanup; this is the one with an
/// injectable seam.)
#[tokio::test]
async fn lost_race_after_key_creation_deletes_the_key() {
    with_control_db(
        "lost_race_after_key_creation_deletes_the_key",
        |url| async move {
            let (control, cluster) = setup(&url).await;

            // An API whose create_key handler deletes the accounts row
            // before returning: the credentials insert that follows lands
            // on a missing FK target.
            struct YankingApi {
                inner: RecordingProvisioning,
                control: ControlPlane,
                subdomain: String,
                /// The account id, recovered from the key name (the only
                /// place provision_account leaks it before failing) so the
                /// test can find the orphaned tenant database afterwards.
                seen_account_id: std::sync::Mutex<Option<String>>,
            }
            #[async_trait::async_trait]
            impl atomic_cloud::ProvisioningApi for YankingApi {
                async fn create_key(
                    &self,
                    name: &str,
                    credit_limit_cents: u32,
                    monthly_reset: bool,
                ) -> Result<atomic_cloud::CreatedRuntimeKey, CloudError> {
                    *self.seen_account_id.lock().expect("account id lock") =
                        name.strip_prefix("atomic-cloud/").map(ToString::to_string);
                    let created = self
                        .inner
                        .create_key(name, credit_limit_cents, monthly_reset)
                        .await?;
                    // The concurrent deletion lands here: the credentials
                    // insert provision is about to attempt has no FK target
                    // left.
                    sqlx::query("DELETE FROM accounts WHERE subdomain = $1")
                        .bind(&self.subdomain)
                        .execute(self.control.pool())
                        .await
                        .expect("yank accounts row");
                    Ok(created)
                }
                async fn update_key_limit(
                    &self,
                    external_key_id: &str,
                    credit_limit_cents: u32,
                ) -> Result<(), CloudError> {
                    self.inner
                        .update_key_limit(external_key_id, credit_limit_cents)
                        .await
                }
                async fn delete_key(&self, external_key_id: &str) -> Result<(), CloudError> {
                    self.inner.delete_key(external_key_id).await
                }
                async fn get_key_usage(
                    &self,
                    external_key_id: &str,
                ) -> Result<atomic_cloud::RuntimeKeyUsage, CloudError> {
                    self.inner.get_key_usage(external_key_id).await
                }
            }

            let api = Arc::new(YankingApi {
                inner: RecordingProvisioning::default(),
                control: control.clone(),
                subdomain: "yanked".to_string(),
                seen_account_id: std::sync::Mutex::new(None),
            });
            let managed = ManagedKeys::Enabled {
                api: Arc::clone(&api) as Arc<dyn atomic_cloud::ProvisioningApi>,
                vault: Arc::new(test_vault()),
                config: atomic_cloud::ManagedKeyConfig::default(),
            };

            let err = provision_account(
                &control,
                &cluster,
                &managed,
                new_account("k@example.com", "yanked"),
            )
            .await
            .expect_err("the lost race must fail the provision");
            assert!(
                matches!(err, CloudError::Database { .. }),
                "the credentials insert hits the missing FK target, got: {err:?}"
            );

            // The minted key was deleted with the locally held id; nothing
            // references it anywhere.
            let (_, external_key_id) = RecordingProvisioning::nth_key(0);
            assert_eq!(api.inner.deleted_key_ids(), vec![external_key_id]);
            let leftover_rows: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM provider_credentials")
                    .fetch_one(control.pool())
                    .await
                    .expect("count credentials");
            assert_eq!(leftover_rows, 0);

            // The tenant database created before step 9 is now classic
            // orphan debris (no control-plane row references it — which is
            // also why with_control_db's cleanup can't see it). The
            // reaper's orphan arm is its designated reclaimer; run a pass
            // and prove the database is gone.
            let account_id = api
                .seen_account_id
                .lock()
                .expect("account id lock")
                .clone()
                .expect("create_key saw the account id");
            let orphan_db =
                tenant_db_name(Uuid::parse_str(&account_id).expect("account id is a UUID"));
            let summary = run_reaper_pass(
                &control,
                &cluster,
                &ManagedKeys::Disabled,
                &ReaperPolicy::default(),
            )
            .await;
            assert!(
                summary.orphan_dbs_dropped.contains(&orphan_db),
                "orphan arm must reclaim {orphan_db}: {summary:?}"
            );
        },
    )
    .await;
}

/// The double-mint race inside step 9: two concurrent resumes can both pass
/// the existence check and both mint a key. The row insert is conditional
/// (`ON CONFLICT DO NOTHING`), so the loser must detect the conflict,
/// delete the key *it* just minted (instead of silently overwriting the
/// winner's `external_key_id`, which would orphan a billed key with no
/// trace), and proceed on the winner's row. Deterministic via the storage
/// seam: the recording API's `create_key` plants the winner's row between
/// the loser's create and its insert.
#[tokio::test]
async fn double_mint_race_loser_deletes_its_own_key_and_uses_the_winner() {
    with_control_db(
        "double_mint_race_loser_deletes_its_own_key_and_uses_the_winner",
        |url| async move {
            let (control, cluster) = setup(&url).await;

            /// Plants the concurrent winner's credentials row inside
            /// `create_key`, after the inner mint succeeds — exactly the
            /// interleaving where both resumes passed the existence check.
            struct RacingApi {
                inner: RecordingProvisioning,
                control: ControlPlane,
            }
            #[async_trait::async_trait]
            impl atomic_cloud::ProvisioningApi for RacingApi {
                async fn create_key(
                    &self,
                    name: &str,
                    credit_limit_cents: u32,
                    monthly_reset: bool,
                ) -> Result<atomic_cloud::CreatedRuntimeKey, CloudError> {
                    let created = self
                        .inner
                        .create_key(name, credit_limit_cents, monthly_reset)
                        .await?;
                    let account_id = name
                        .strip_prefix("atomic-cloud/")
                        .expect("managed key names are account-keyed");
                    // The "winner" resume's insert lands here.
                    let landed = atomic_cloud::insert_credentials_if_absent(
                        &self.control,
                        &test_vault(),
                        account_id,
                        NewCredentials {
                            provider: Provider::OpenRouter,
                            origin: CredentialOrigin::Managed,
                            api_key: SecretKey::new("sk-or-v1-winner".to_string()),
                            external_key_id: Some("orkey-winner".to_string()),
                            model_config: serde_json::json!({}),
                        },
                    )
                    .await
                    .expect("plant winner row");
                    assert!(landed, "the planted winner row must be first");
                    Ok(created)
                }
                async fn update_key_limit(
                    &self,
                    external_key_id: &str,
                    credit_limit_cents: u32,
                ) -> Result<(), CloudError> {
                    self.inner
                        .update_key_limit(external_key_id, credit_limit_cents)
                        .await
                }
                async fn delete_key(&self, external_key_id: &str) -> Result<(), CloudError> {
                    self.inner.delete_key(external_key_id).await
                }
                async fn get_key_usage(
                    &self,
                    external_key_id: &str,
                ) -> Result<atomic_cloud::RuntimeKeyUsage, CloudError> {
                    self.inner.get_key_usage(external_key_id).await
                }
            }

            let api = Arc::new(RacingApi {
                inner: RecordingProvisioning::default(),
                control: control.clone(),
            });
            let managed = ManagedKeys::Enabled {
                api: Arc::clone(&api) as Arc<dyn atomic_cloud::ProvisioningApi>,
                vault: Arc::new(test_vault()),
                config: atomic_cloud::ManagedKeyConfig::default(),
            };

            // The losing resume completes the provision anyway — on the
            // winner's key.
            let account = provision_account(
                &control,
                &cluster,
                &managed,
                new_account("k@example.com", "raced"),
            )
            .await
            .expect("the losing resume must still complete");
            assert_eq!(
                account_status(&control, &account.account_id)
                    .await
                    .as_deref(),
                Some("active")
            );

            // The loser deleted exactly the key IT minted — never the
            // winner's.
            let (_, loser_key_id) = RecordingProvisioning::nth_key(0);
            assert_eq!(api.inner.deleted_key_ids(), vec![loser_key_id]);

            // The stored row is the winner's, untouched, and active.
            let creds = get_credentials(
                &control,
                &test_vault(),
                &account.account_id,
                Provider::OpenRouter,
                CredentialOrigin::Managed,
            )
            .await
            .expect("fetch")
            .expect("winner row survives");
            assert_eq!(creds.external_key_id.as_deref(), Some("orkey-winner"));
            assert_eq!(creds.api_key.expose(), "sk-or-v1-winner");
            assert_eq!(credentials_count(&control, &account.account_id).await, 1);
            assert_eq!(
                active_provider(&control, &account.account_id).await,
                Some(("openrouter".to_string(), "managed".to_string()))
            );
        },
    )
    .await;
}
