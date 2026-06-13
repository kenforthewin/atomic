//! Magic-link issuance/consumption integration tests: hash-only storage,
//! single-use purpose-pinned consumption, and the inertness of expired and
//! wrong-purpose rows.
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command. Control plane only — no tenant
//! databases, no HTTP (the request-link routes are covered end-to-end in
//! `tests/account_plane.rs`).

mod support;

use std::time::Duration;

use atomic_cloud::{consume_magic_link, issue_magic_link, ControlPlane, MagicLinkPurpose};
use sha2::{Digest, Sha256};
use support::{control_db_contains, with_control_db};

async fn setup(control_url: &str) -> ControlPlane {
    let control = ControlPlane::connect(control_url)
        .await
        .expect("connect control plane");
    control.initialize().await.expect("migrate control plane");
    control
}

fn sha256_hex(plaintext: &str) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(plaintext.as_bytes()))
}

#[tokio::test]
async fn issuance_stores_hash_only() {
    with_control_db("issuance_stores_hash_only", |url| async move {
        let control = setup(&url).await;

        let plaintext = issue_magic_link(
            &control,
            "kenny@example.com",
            MagicLinkPurpose::Signup,
            Some("kenny"),
            Some("203.0.113.7"),
            Duration::from_secs(15 * 60),
        )
        .await
        .expect("issue magic link");
        assert!(plaintext.starts_with("aml_"), "plaintext is aml_<random>");
        // 32 bytes -> 52 base32 chars; plus the 4-char prefix.
        assert_eq!(plaintext.len(), 4 + 52);

        // The row is keyed by the SHA-256 of the plaintext, with the
        // request recorded around it.
        let (email, purpose, subdomain, ip, consumed): (
            String,
            String,
            Option<String>,
            Option<String>,
            Option<chrono::DateTime<chrono::Utc>>,
        ) = sqlx::query_as(
            "SELECT email, purpose, requested_subdomain, request_ip, consumed_at \
             FROM magic_links WHERE token_hash = $1",
        )
        .bind(sha256_hex(&plaintext))
        .fetch_one(control.pool())
        .await
        .expect("row exists under the plaintext's hash");
        assert_eq!(email, "kenny@example.com");
        assert_eq!(purpose, "signup");
        assert_eq!(subdomain.as_deref(), Some("kenny"));
        assert_eq!(ip.as_deref(), Some("203.0.113.7"));
        assert!(consumed.is_none());

        // The link plaintext never touches the database: no text column in
        // any control-plane table contains an `aml_` substring (the hash is
        // lowercase hex, which cannot even contain 'm').
        assert!(
            !control_db_contains(&url, "aml_").await,
            "no aml_ substring may appear anywhere in the control database"
        );
    })
    .await;
}

#[tokio::test]
async fn consumption_is_single_use() {
    with_control_db("consumption_is_single_use", |url| async move {
        let control = setup(&url).await;

        let plaintext = issue_magic_link(
            &control,
            "kenny@example.com",
            MagicLinkPurpose::Login,
            None,
            None,
            Duration::from_secs(15 * 60),
        )
        .await
        .expect("issue magic link");

        // The wrong purpose consumes nothing AND burns nothing: the purpose
        // pin lives inside the UPDATE's WHERE clause, so a login link
        // presented to the signup endpoint stays live for the login one.
        assert!(
            consume_magic_link(&control, &plaintext, MagicLinkPurpose::Signup)
                .await
                .expect("consume query")
                .is_none(),
            "a login link must not consume as a signup link"
        );

        // First correct-purpose click wins, with the row intact.
        let record = consume_magic_link(&control, &plaintext, MagicLinkPurpose::Login)
            .await
            .expect("consume query")
            .expect("fresh link consumes despite the earlier wrong-purpose attempt");
        assert_eq!(record.email, "kenny@example.com");
        assert_eq!(record.purpose, MagicLinkPurpose::Login);
        assert_eq!(record.requested_subdomain, None);
        assert!(
            record.consumed_at.is_some(),
            "consumption stamps consumed_at in the same statement"
        );

        // Second click is inert.
        assert!(
            consume_magic_link(&control, &plaintext, MagicLinkPurpose::Login)
                .await
                .expect("consume query")
                .is_none(),
            "a consumed link must never consume again"
        );

        // Garbage never consumes.
        assert!(
            consume_magic_link(&control, "aml_nonsense", MagicLinkPurpose::Login)
                .await
                .expect("consume query")
                .is_none()
        );
    })
    .await;
}

/// Expired rows are inert: the consume UPDATE's WHERE clause
/// (`consumed_at IS NULL AND expires_at > NOW()`) skips them, and skipping
/// must not mutate them — `consumed_at` stays NULL, pinned by direct SQL.
#[tokio::test]
async fn expired_links_are_inert() {
    with_control_db("expired_links_are_inert", |url| async move {
        let control = setup(&url).await;

        let plaintext = issue_magic_link(
            &control,
            "kenny@example.com",
            MagicLinkPurpose::Signup,
            Some("kenny"),
            None,
            Duration::from_secs(15 * 60),
        )
        .await
        .expect("issue magic link");
        let hash = sha256_hex(&plaintext);

        sqlx::query(
            "UPDATE magic_links SET expires_at = NOW() - INTERVAL '1 minute' \
                     WHERE token_hash = $1",
        )
        .bind(&hash)
        .execute(control.pool())
        .await
        .expect("expire link");

        assert!(
            consume_magic_link(&control, &plaintext, MagicLinkPurpose::Signup)
                .await
                .expect("consume query")
                .is_none(),
            "an expired link must not consume"
        );

        // The failed consumption left the row untouched.
        let consumed: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT consumed_at FROM magic_links WHERE token_hash = $1")
                .bind(&hash)
                .fetch_one(control.pool())
                .await
                .expect("read row back");
        assert!(
            consumed.is_none(),
            "a refused consumption must not stamp consumed_at"
        );
    })
    .await;
}
