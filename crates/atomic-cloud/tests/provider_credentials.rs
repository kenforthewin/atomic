//! Encrypted provider-credential store integration tests (plan: "Provider
//! management" → "Storage schema" / "Encryption at rest").
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command. Pure-crypto behavior (AAD binding,
//! nonce freshness, master-key parsing) is unit-tested in `src/keyvault.rs`;
//! these tests cover the store: CRUD against the real schema, the
//! active-provider pointer invariants, end-to-end row binding, and the
//! "plaintext never at rest" guarantee.

mod support;

use atomic_cloud::{
    delete_credentials, get_active_credentials, get_credentials, record_validation,
    set_active_provider, touch_last_used, upsert_credentials, CloudError, ControlPlane,
    CredentialOrigin, EnvMasterKeyVault, NewCredentials, Provider, SecretKey,
};
use support::{control_db_contains, with_control_db};

/// A managed-key plaintext distinctive enough that finding it anywhere in
/// the control database is unambiguous.
const MANAGED_KEY: &str = "sk-or-v1-managed-3f9a1c-secret";

fn vault() -> EnvMasterKeyVault {
    EnvMasterKeyVault::new([7u8; 32])
}

async fn connect(url: &str) -> ControlPlane {
    let control = ControlPlane::connect(url).await.expect("connect");
    control.initialize().await.expect("migrate");
    control
}

/// Insert a minimal accounts row to satisfy the FK.
async fn insert_account(control: &ControlPlane, id: &str, subdomain: &str) {
    sqlx::query(
        "INSERT INTO accounts (id, subdomain, email, status, plan) \
         VALUES ($1, $2, 'k@example.com', 'active', 'free')",
    )
    .bind(id)
    .bind(subdomain)
    .execute(control.pool())
    .await
    .expect("insert account");
}

fn managed_openrouter(key: &str) -> NewCredentials {
    NewCredentials {
        provider: Provider::OpenRouter,
        origin: CredentialOrigin::Managed,
        api_key: SecretKey::new(key.to_string()),
        external_key_id: Some("or-key-abc123".to_string()),
        model_config: serde_json::json!({
            "embedding_model": "openai/text-embedding-3-small",
            "llm_model": "anthropic/claude-haiku",
        }),
    }
}

#[tokio::test]
async fn upsert_get_roundtrip_never_stores_plaintext() {
    with_control_db(
        "upsert_get_roundtrip_never_stores_plaintext",
        |url| async move {
            let control = connect(&url).await;
            let vault = vault();
            insert_account(&control, "acct-1", "kenny").await;

            upsert_credentials(&control, &vault, "acct-1", managed_openrouter(MANAGED_KEY))
                .await
                .expect("upsert");

            let creds = get_credentials(
                &control,
                &vault,
                "acct-1",
                Provider::OpenRouter,
                CredentialOrigin::Managed,
            )
            .await
            .expect("get")
            .expect("row exists");

            assert_eq!(creds.api_key.expose(), MANAGED_KEY);
            assert_eq!(creds.account_id, "acct-1");
            assert_eq!(creds.provider, Provider::OpenRouter);
            assert_eq!(creds.origin, CredentialOrigin::Managed);
            assert_eq!(creds.external_key_id.as_deref(), Some("or-key-abc123"));
            assert_eq!(
                creds.model_config["llm_model"],
                serde_json::json!("anthropic/claude-haiku")
            );
            assert!(creds.rotated_at.is_none(), "fresh insert is not a rotation");
            assert!(creds.last_used_at.is_none());
            assert!(creds.last_validated_at.is_none());
            assert!(creds.last_validation_error.is_none());

            // Debug of the whole record redacts the key (SecretKey).
            let rendered = format!("{creds:?}");
            assert!(rendered.contains("[redacted]"));
            assert!(!rendered.contains(MANAGED_KEY), "Debug leaked the key");

            // The plaintext appears in no text column of any table — and
            // not in the BYTEA either (raw AES-GCM output can't contain it,
            // but pin the property, not the implementation).
            assert!(
                !control_db_contains(&url, MANAGED_KEY).await,
                "plaintext key found at rest in a text column"
            );
            let in_bytea: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM provider_credentials \
                 WHERE position($1::bytea in encrypted_key) > 0)",
            )
            .bind(MANAGED_KEY.as_bytes())
            .fetch_one(control.pool())
            .await
            .expect("scan encrypted_key");
            assert!(!in_bytea, "plaintext key found inside encrypted_key");

            // Missing rows are None, not errors.
            let absent = get_credentials(
                &control,
                &vault,
                "acct-1",
                Provider::OpenAiCompat,
                CredentialOrigin::User,
            )
            .await
            .expect("get absent");
            assert!(absent.is_none());
        },
    )
    .await;
}

#[tokio::test]
async fn upsert_replacement_is_a_rotation() {
    with_control_db("upsert_replacement_is_a_rotation", |url| async move {
        let control = connect(&url).await;
        let vault = vault();
        insert_account(&control, "acct-1", "kenny").await;

        upsert_credentials(&control, &vault, "acct-1", managed_openrouter("old-key"))
            .await
            .expect("first upsert");
        // Give the row some validation history to prove replacement resets it.
        record_validation(
            &control,
            "acct-1",
            Provider::OpenRouter,
            CredentialOrigin::Managed,
            None,
        )
        .await
        .expect("record success");

        let mut replacement = managed_openrouter("new-key");
        replacement.external_key_id = Some("or-key-def456".to_string());
        upsert_credentials(&control, &vault, "acct-1", replacement)
            .await
            .expect("replacing upsert");

        let creds = get_credentials(
            &control,
            &vault,
            "acct-1",
            Provider::OpenRouter,
            CredentialOrigin::Managed,
        )
        .await
        .expect("get")
        .expect("row exists");
        assert_eq!(creds.api_key.expose(), "new-key");
        assert_eq!(creds.external_key_id.as_deref(), Some("or-key-def456"));
        assert!(creds.rotated_at.is_some(), "replacement stamps rotated_at");
        assert!(
            creds.last_validated_at.is_none(),
            "validation state describes the stored key; reset on rotation"
        );
    })
    .await;
}

#[tokio::test]
async fn active_provider_flip_selects_among_rows() {
    with_control_db(
        "active_provider_flip_selects_among_rows",
        |url| async move {
            let control = connect(&url).await;
            let vault = vault();
            insert_account(&control, "acct-1", "kenny").await;

            // No pointer yet: active is None, not an error.
            assert!(get_active_credentials(&control, &vault, "acct-1")
                .await
                .expect("get active")
                .is_none());

            // Flipping to a row that doesn't exist is refused, typed.
            let refused = set_active_provider(
                &control,
                "acct-1",
                Some((Provider::OpenRouter, CredentialOrigin::Managed)),
            )
            .await;
            assert!(matches!(
                refused,
                Err(CloudError::MissingProviderCredentials { .. })
            ));

            // Managed and BYOK rows coexist; the pointer selects between them.
            upsert_credentials(&control, &vault, "acct-1", managed_openrouter(MANAGED_KEY))
                .await
                .expect("upsert managed");
            upsert_credentials(
                &control,
                &vault,
                "acct-1",
                NewCredentials {
                    provider: Provider::OpenAiCompat,
                    origin: CredentialOrigin::User,
                    api_key: SecretKey::new("byok-key".to_string()),
                    external_key_id: None,
                    model_config: serde_json::json!({ "llm_model": "gpt-omni" }),
                },
            )
            .await
            .expect("upsert byok");

            set_active_provider(
                &control,
                "acct-1",
                Some((Provider::OpenRouter, CredentialOrigin::Managed)),
            )
            .await
            .expect("flip to managed");
            let active = get_active_credentials(&control, &vault, "acct-1")
                .await
                .expect("get active")
                .expect("active row");
            assert_eq!(active.origin, CredentialOrigin::Managed);
            assert_eq!(active.api_key.expose(), MANAGED_KEY);

            set_active_provider(
                &control,
                "acct-1",
                Some((Provider::OpenAiCompat, CredentialOrigin::User)),
            )
            .await
            .expect("flip to byok");
            let active = get_active_credentials(&control, &vault, "acct-1")
                .await
                .expect("get active")
                .expect("active row");
            assert_eq!(active.provider, Provider::OpenAiCompat);
            assert_eq!(active.origin, CredentialOrigin::User);
            assert_eq!(active.api_key.expose(), "byok-key");

            // Clearing the pointer works; clearing a missing account fails loudly.
            set_active_provider(&control, "acct-1", None)
                .await
                .expect("clear");
            assert!(get_active_credentials(&control, &vault, "acct-1")
                .await
                .expect("get active")
                .is_none());
            assert!(set_active_provider(&control, "no-such-account", None)
                .await
                .is_err());
        },
    )
    .await;
}

#[tokio::test]
async fn delete_clears_active_pointer_and_reports_existence() {
    with_control_db(
        "delete_clears_active_pointer_and_reports_existence",
        |url| async move {
            let control = connect(&url).await;
            let vault = vault();
            insert_account(&control, "acct-1", "kenny").await;
            upsert_credentials(&control, &vault, "acct-1", managed_openrouter(MANAGED_KEY))
                .await
                .expect("upsert");
            set_active_provider(
                &control,
                "acct-1",
                Some((Provider::OpenRouter, CredentialOrigin::Managed)),
            )
            .await
            .expect("flip");

            let deleted = delete_credentials(
                &control,
                "acct-1",
                Provider::OpenRouter,
                CredentialOrigin::Managed,
            )
            .await
            .expect("delete");
            assert!(deleted, "first delete removes the row");

            // The pointer was cleared with the row — both halves NULL.
            let (active_provider, active_origin): (Option<String>, Option<String>) =
                sqlx::query_as("SELECT active_provider, active_origin FROM accounts WHERE id = $1")
                    .bind("acct-1")
                    .fetch_one(control.pool())
                    .await
                    .expect("read pointer");
            assert_eq!(active_provider, None);
            assert_eq!(active_origin, None);
            assert!(get_active_credentials(&control, &vault, "acct-1")
                .await
                .expect("get active")
                .is_none());

            let deleted_again = delete_credentials(
                &control,
                "acct-1",
                Provider::OpenRouter,
                CredentialOrigin::Managed,
            )
            .await
            .expect("second delete");
            assert!(!deleted_again, "second delete reports no row");
        },
    )
    .await;
}

#[tokio::test]
async fn usage_and_validation_stamps() {
    with_control_db("usage_and_validation_stamps", |url| async move {
        let control = connect(&url).await;
        let vault = vault();
        insert_account(&control, "acct-1", "kenny").await;
        upsert_credentials(&control, &vault, "acct-1", managed_openrouter(MANAGED_KEY))
            .await
            .expect("upsert");

        let fetch = || async {
            get_credentials(
                &control,
                &vault,
                "acct-1",
                Provider::OpenRouter,
                CredentialOrigin::Managed,
            )
            .await
            .expect("get")
            .expect("row exists")
        };

        touch_last_used(
            &control,
            "acct-1",
            Provider::OpenRouter,
            CredentialOrigin::Managed,
        )
        .await
        .expect("touch");
        assert!(fetch().await.last_used_at.is_some());

        // Failure stores the message but does NOT stamp last_validated_at:
        // the timestamp means "last successful validation".
        record_validation(
            &control,
            "acct-1",
            Provider::OpenRouter,
            CredentialOrigin::Managed,
            Some("401 unauthorized from provider"),
        )
        .await
        .expect("record failure");
        let creds = fetch().await;
        assert_eq!(
            creds.last_validation_error.as_deref(),
            Some("401 unauthorized from provider")
        );
        assert!(creds.last_validated_at.is_none());

        // Success stamps the timestamp and clears the error.
        record_validation(
            &control,
            "acct-1",
            Provider::OpenRouter,
            CredentialOrigin::Managed,
            None,
        )
        .await
        .expect("record success");
        let creds = fetch().await;
        assert!(creds.last_validated_at.is_some());
        assert!(creds.last_validation_error.is_none());
    })
    .await;
}

#[tokio::test]
async fn rows_are_bound_to_their_account_and_cascade_with_it() {
    with_control_db(
        "rows_are_bound_to_their_account_and_cascade_with_it",
        |url| async move {
            let control = connect(&url).await;
            let vault = vault();
            insert_account(&control, "acct-a", "alpha").await;
            insert_account(&control, "acct-b", "bravo").await;
            upsert_credentials(&control, &vault, "acct-a", managed_openrouter(MANAGED_KEY))
                .await
                .expect("upsert for a");

            // Adversarially copy A's encrypted row onto B (something the
            // store API never does). Decryption must fail: the vault's AAD
            // binds the ciphertext to (account_id, provider), and that
            // binding has to hold end-to-end through the store, not just in
            // keyvault unit tests.
            sqlx::query(
                "INSERT INTO provider_credentials \
                     (account_id, provider, origin, external_key_id, encrypted_key, \
                      nonce, encryption_version, model_config) \
                 SELECT 'acct-b', provider, origin, external_key_id, encrypted_key, \
                        nonce, encryption_version, model_config \
                 FROM provider_credentials WHERE account_id = 'acct-a'",
            )
            .execute(control.pool())
            .await
            .expect("copy row across accounts");

            let smuggled = get_credentials(
                &control,
                &vault,
                "acct-b",
                Provider::OpenRouter,
                CredentialOrigin::Managed,
            )
            .await;
            assert!(
                matches!(smuggled, Err(CloudError::CredentialDecrypt(_))),
                "a ciphertext moved across accounts must fail authentication, got {smuggled:?}"
            );

            // FK CASCADE (slice-1 convention): deleting the account removes
            // its credential rows.
            sqlx::query("DELETE FROM accounts WHERE id = 'acct-a'")
                .execute(control.pool())
                .await
                .expect("delete account");
            let remaining: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM provider_credentials WHERE account_id = 'acct-a'",
            )
            .fetch_one(control.pool())
            .await
            .expect("count rows");
            assert_eq!(remaining, 0, "credentials cascade with the account");
        },
    )
    .await;
}

#[tokio::test]
async fn schema_rejects_unknown_vocabulary_and_half_set_pointers() {
    with_control_db(
        "schema_rejects_unknown_vocabulary_and_half_set_pointers",
        |url| async move {
            let control = connect(&url).await;
            insert_account(&control, "acct-1", "kenny").await;

            // CHECK constraints pin the provider/origin vocabulary at the
            // schema, beneath the Rust enums.
            let bad_provider = sqlx::query(
                "INSERT INTO provider_credentials \
                     (account_id, provider, origin, encrypted_key, nonce, \
                      encryption_version, model_config) \
                 VALUES ('acct-1', 'ollama', 'managed', '\\x00', '\\x00', 1, '{}')",
            )
            .execute(control.pool())
            .await;
            assert!(bad_provider.is_err(), "provider CHECK must reject 'ollama'");

            let bad_origin = sqlx::query(
                "INSERT INTO provider_credentials \
                     (account_id, provider, origin, encrypted_key, nonce, \
                      encryption_version, model_config) \
                 VALUES ('acct-1', 'openrouter', 'platform', '\\x00', '\\x00', 1, '{}')",
            )
            .execute(control.pool())
            .await;
            assert!(bad_origin.is_err(), "origin CHECK must reject 'platform'");

            // The paired-NULL CHECK rejects a half-set active pointer.
            let half_set =
                sqlx::query("UPDATE accounts SET active_provider = 'openrouter' WHERE id = $1")
                    .bind("acct-1")
                    .execute(control.pool())
                    .await;
            assert!(
                half_set.is_err(),
                "active_provider without active_origin must violate the paired CHECK"
            );
        },
    )
    .await;
}
