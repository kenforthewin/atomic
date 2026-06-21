//! Token and session issuance/verification integration tests.
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command. These exercise the control plane only —
//! no tenant databases are provisioned, the accounts rows are inserted
//! directly.

mod support;

use std::time::Duration;

use atomic_cloud::{
    create_session, issue_token, verify_session, verify_token, ControlPlane, TokenScope,
};
use support::with_control_db;

const ACCOUNT_A: &str = "11111111-1111-4111-8111-111111111111";
const ACCOUNT_B: &str = "22222222-2222-4222-8222-222222222222";

/// Migrated control plane with two active accounts to verify cross-account
/// scoping against.
async fn setup(control_url: &str) -> ControlPlane {
    let control = ControlPlane::connect(
        control_url,
        atomic_cloud::control_plane::DEFAULT_CONTROL_POOL_MAX_CONNECTIONS,
    )
    .await
    .expect("connect control plane");
    control.initialize().await.expect("migrate control plane");
    for (id, subdomain) in [(ACCOUNT_A, "alpha"), (ACCOUNT_B, "beta")] {
        sqlx::query(
            "INSERT INTO accounts (id, subdomain, email, status, plan) \
             VALUES ($1, $2, $3, 'active', 'free')",
        )
        .bind(id)
        .bind(subdomain)
        .bind(format!("{subdomain}@example.com"))
        .execute(control.pool())
        .await
        .expect("insert account");
    }
    control
}

#[tokio::test]
async fn token_roundtrip_and_scoping() {
    with_control_db("token_roundtrip_and_scoping", |url| async move {
        let control = setup(&url).await;

        let plaintext = issue_token(
            &control,
            ACCOUNT_A,
            TokenScope::Database,
            Some("work-kb"),
            "my-laptop",
        )
        .await
        .expect("issue token");
        assert!(plaintext.starts_with("atm_"), "plaintext is atm_<random>");

        // The plaintext is never persisted — only its SHA-256.
        let plaintext_stored: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM cloud_tokens WHERE hash = $1)")
                .bind(&plaintext)
                .fetch_one(control.pool())
                .await
                .expect("query cloud_tokens");
        assert!(
            !plaintext_stored,
            "plaintext must not appear in cloud_tokens"
        );

        // Scope is serialized as text.
        let stored_scope: String =
            sqlx::query_scalar("SELECT scope FROM cloud_tokens WHERE account_id = $1")
                .bind(ACCOUNT_A)
                .fetch_one(control.pool())
                .await
                .expect("read stored scope");
        assert_eq!(stored_scope, "database");

        // Happy path: verifies for its own account, with all fields intact.
        let record = verify_token(&control, ACCOUNT_A, &plaintext)
            .await
            .expect("verify query")
            .expect("token verifies for its own account");
        assert_eq!(record.account_id, ACCOUNT_A);
        assert_eq!(record.scope, TokenScope::Database);
        assert_eq!(record.allowed_db_id.as_deref(), Some("work-kb"));
        assert_eq!(record.name, "my-laptop");
        assert!(
            record.last_used_at.is_some(),
            "verification stamps last_used_at"
        );
        assert!(record.revoked_at.is_none());
        assert!(record.expires_at.is_none());

        // Cross-account chokepoint: the same plaintext is worthless for
        // any other account.
        assert!(
            verify_token(&control, ACCOUNT_B, &plaintext)
                .await
                .expect("verify query")
                .is_none(),
            "a token must not verify for another account"
        );

        // Garbage never verifies.
        assert!(verify_token(&control, ACCOUNT_A, "atm_nonsense")
            .await
            .expect("verify query")
            .is_none());

        // Revocation kills it.
        sqlx::query("UPDATE cloud_tokens SET revoked_at = NOW() WHERE hash = $1")
            .bind(&record.hash)
            .execute(control.pool())
            .await
            .expect("revoke token");
        assert!(
            verify_token(&control, ACCOUNT_A, &plaintext)
                .await
                .expect("verify query")
                .is_none(),
            "revoked token must not verify"
        );
    })
    .await;
}

#[tokio::test]
async fn token_expiry() {
    with_control_db("token_expiry", |url| async move {
        let control = setup(&url).await;

        let plaintext = issue_token(&control, ACCOUNT_A, TokenScope::Account, None, "expiring")
            .await
            .expect("issue token");

        // A future expiry still verifies...
        sqlx::query(
            "UPDATE cloud_tokens SET expires_at = NOW() + INTERVAL '1 hour' \
             WHERE account_id = $1",
        )
        .bind(ACCOUNT_A)
        .execute(control.pool())
        .await
        .expect("set future expiry");
        assert!(verify_token(&control, ACCOUNT_A, &plaintext)
            .await
            .expect("verify query")
            .is_some());

        // ...a past one doesn't.
        sqlx::query(
            "UPDATE cloud_tokens SET expires_at = NOW() - INTERVAL '1 hour' \
             WHERE account_id = $1",
        )
        .bind(ACCOUNT_A)
        .execute(control.pool())
        .await
        .expect("set past expiry");
        assert!(
            verify_token(&control, ACCOUNT_A, &plaintext)
                .await
                .expect("verify query")
                .is_none(),
            "expired token must not verify"
        );
    })
    .await;
}

#[tokio::test]
async fn session_roundtrip_and_expiry() {
    with_control_db("session_roundtrip_and_expiry", |url| async move {
        let control = setup(&url).await;

        let plaintext = create_session(
            &control,
            ACCOUNT_A,
            Duration::from_secs(3600),
            Some("203.0.113.7"),
            Some("test-agent/1.0"),
        )
        .await
        .expect("create session");
        assert!(
            plaintext.starts_with("ats_"),
            "sessions carry their own prefix"
        );

        // Plaintext never persisted.
        let plaintext_stored: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE hash = $1)")
                .bind(&plaintext)
                .fetch_one(control.pool())
                .await
                .expect("query sessions");
        assert!(!plaintext_stored, "plaintext must not appear in sessions");

        let record = verify_session(&control, ACCOUNT_A, &plaintext)
            .await
            .expect("verify query")
            .expect("session verifies for its own account");
        assert_eq!(record.account_id, ACCOUNT_A);
        assert_eq!(record.ip_first_seen.as_deref(), Some("203.0.113.7"));
        assert_eq!(record.ua_first_seen.as_deref(), Some("test-agent/1.0"));
        assert!(record.expires_at > chrono::Utc::now());

        // Cross-account chokepoint — the shared cookie domain makes this
        // the line that actually isolates tenants.
        assert!(verify_session(&control, ACCOUNT_B, &plaintext)
            .await
            .expect("verify query")
            .is_none());

        // Garbage never verifies.
        assert!(verify_session(&control, ACCOUNT_A, "ats_nonsense")
            .await
            .expect("verify query")
            .is_none());

        // Expiry.
        sqlx::query("UPDATE sessions SET expires_at = NOW() - INTERVAL '1 minute' WHERE hash = $1")
            .bind(&record.hash)
            .execute(control.pool())
            .await
            .expect("expire session");
        assert!(
            verify_session(&control, ACCOUNT_A, &plaintext)
                .await
                .expect("verify query")
                .is_none(),
            "expired session must not verify"
        );
    })
    .await;
}
