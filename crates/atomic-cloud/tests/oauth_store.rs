//! Per-account OAuth control-plane storage tests (plan: "Auth & tenant
//! routing" → control-plane schema `oauth_clients`/`oauth_codes`, "OAuth").
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command. These exercise the control plane only —
//! no tenant databases are provisioned, the accounts rows are inserted
//! directly. The invariant under test is the cross-tenant chokepoint: every
//! query is scoped by `account_id`, so account B can never reach account A's
//! OAuth client or authorization code.

mod support;

use std::time::Duration;

use atomic_cloud::{
    consume_oauth_code, create_oauth_client, get_oauth_client, insert_oauth_code,
    purge_expired_oauth_codes, record_oauth_code_token, ControlPlane, NewOAuthCode, OAuthCode,
    TokenScope, OAUTH_CLIENT_PREFIX, OAUTH_CODE_PREFIX, OAUTH_CODE_TTL,
};
use support::{control_db_contains, with_control_db};

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

/// Issue an account-scoped authorization code for `account_id` under `client`
/// with the given challenge, using the default TTL. Returns the plaintext.
async fn issue_account_code(
    control: &ControlPlane,
    account_id: &str,
    client_id: &str,
    challenge: &str,
) -> String {
    insert_oauth_code(
        control,
        NewOAuthCode {
            account_id,
            client_id,
            code_challenge: challenge,
            code_challenge_method: "S256",
            redirect_uri: "http://localhost:3000/callback",
            scope: TokenScope::Account,
            allowed_db_id: None,
        },
        OAUTH_CODE_TTL,
    )
    .await
    .expect("insert oauth code")
}

#[tokio::test]
async fn client_create_get_roundtrip_and_account_scope() {
    with_control_db(
        "client_create_get_roundtrip_and_account_scope",
        |url| async move {
            let control = setup(&url).await;

            let redirect_uris = vec![
                "http://localhost:3000/callback".to_string(),
                "https://claude.ai/api/mcp/auth_callback".to_string(),
            ];
            let (client_id, secret) =
                create_oauth_client(&control, ACCOUNT_A, "Claude Desktop", &redirect_uris)
                    .await
                    .expect("register client");

            assert!(
                client_id.starts_with(OAUTH_CLIENT_PREFIX),
                "client_id is occ_<random>: {client_id}"
            );
            assert!(!secret.is_empty(), "DCR returns a client secret");
            assert_ne!(
                secret, client_id,
                "the secret and the public client_id are different values"
            );

            // Roundtrip: the client fetches for its own account, all fields
            // intact and the secret verifiable by hash.
            let fetched = get_oauth_client(&control, ACCOUNT_A, &client_id)
                .await
                .expect("get query")
                .expect("client resolves for its own account");
            assert_eq!(fetched.client_id, client_id);
            assert_eq!(fetched.account_id, ACCOUNT_A);
            assert_eq!(fetched.client_name, "Claude Desktop");
            assert_eq!(fetched.redirect_uris, redirect_uris);
            // The stored hash matches SHA-256 of the once-returned plaintext
            // (the same digest used by tokens/magic_links).
            let expected_hash = data_encoding::HEXLOWER
                .encode(<sha2::Sha256 as sha2::Digest>::digest(secret.as_bytes()).as_slice());
            assert_eq!(
                fetched.client_secret_hash, expected_hash,
                "stored hash is SHA-256 of the issued secret"
            );

            // Cross-account chokepoint: account B cannot resolve account A's
            // client_id, even knowing it verbatim.
            assert!(
                get_oauth_client(&control, ACCOUNT_B, &client_id)
                    .await
                    .expect("get query")
                    .is_none(),
                "a client_id must not resolve under another account"
            );

            // An unknown client_id resolves to nothing.
            assert!(get_oauth_client(&control, ACCOUNT_A, "occ_does_not_exist")
                .await
                .expect("get query")
                .is_none());
        },
    )
    .await;
}

#[tokio::test]
async fn code_insert_consume_is_single_use_and_account_scoped() {
    with_control_db(
        "code_insert_consume_is_single_use_and_account_scoped",
        |url| async move {
            let control = setup(&url).await;
            let (client_id, _) = create_oauth_client(&control, ACCOUNT_A, "Claude Desktop", &[])
                .await
                .expect("register client");

            let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"; // RFC 7636 §A.2
            let code = issue_account_code(&control, ACCOUNT_A, &client_id, challenge).await;
            assert!(
                code.starts_with(OAUTH_CODE_PREFIX),
                "code is oac_<random>: {code}"
            );

            // Cross-account chokepoint: account B cannot consume account A's
            // code, even with the plaintext.
            assert!(
                consume_oauth_code(&control, ACCOUNT_B, &code)
                    .await
                    .expect("consume query")
                    .is_none(),
                "a code must not consume under another account"
            );

            // First consume wins and returns the full row, intact.
            let consumed: OAuthCode = consume_oauth_code(&control, ACCOUNT_A, &code)
                .await
                .expect("consume query")
                .expect("live code consumes once");
            assert_eq!(consumed.account_id, ACCOUNT_A);
            assert_eq!(consumed.client_id, client_id);
            assert_eq!(consumed.code_challenge, challenge);
            assert_eq!(consumed.code_challenge_method, "S256");
            assert_eq!(consumed.redirect_uri, "http://localhost:3000/callback");
            assert_eq!(consumed.scope, TokenScope::Account);
            assert!(consumed.allowed_db_id.is_none());
            assert!(consumed.consumed_at.is_some(), "consume stamps consumed_at");
            assert!(consumed.token_id.is_none());

            // Second consume of the same code returns nothing — single-use.
            assert!(
                consume_oauth_code(&control, ACCOUNT_A, &code)
                    .await
                    .expect("consume query")
                    .is_none(),
                "a code consumes at most once"
            );

            // Garbage never consumes.
            assert!(consume_oauth_code(&control, ACCOUNT_A, "oac_nonsense")
                .await
                .expect("consume query")
                .is_none());
        },
    )
    .await;
}

#[tokio::test]
async fn expired_code_is_not_consumable_and_purges() {
    with_control_db(
        "expired_code_is_not_consumable_and_purges",
        |url| async move {
            let control = setup(&url).await;
            let (client_id, _) = create_oauth_client(&control, ACCOUNT_A, "Claude Desktop", &[])
                .await
                .expect("register client");

            // A zero-TTL code is born expired (expires_at == created_at <=
            // NOW() by the time consume runs).
            let code = insert_oauth_code(
                &control,
                NewOAuthCode {
                    account_id: ACCOUNT_A,
                    client_id: &client_id,
                    code_challenge: "ch",
                    code_challenge_method: "S256",
                    redirect_uri: "http://localhost:3000/callback",
                    scope: TokenScope::Account,
                    allowed_db_id: None,
                },
                Duration::from_secs(0),
            )
            .await
            .expect("insert oauth code");

            assert!(
                consume_oauth_code(&control, ACCOUNT_A, &code)
                    .await
                    .expect("consume query")
                    .is_none(),
                "an expired code must not consume"
            );

            // The reaper's purge removes expired rows.
            let purged = purge_expired_oauth_codes(&control)
                .await
                .expect("purge expired codes");
            assert_eq!(purged, 1, "the one expired code is purged");
            let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oauth_codes")
                .fetch_one(control.pool())
                .await
                .expect("count codes");
            assert_eq!(remaining, 0);
        },
    )
    .await;
}

#[tokio::test]
async fn db_pinned_code_carries_scope_and_records_token() {
    with_control_db(
        "db_pinned_code_carries_scope_and_records_token",
        |url| async move {
            let control = setup(&url).await;
            let (client_id, _) = create_oauth_client(&control, ACCOUNT_A, "Claude Desktop", &[])
                .await
                .expect("register client");

            // A db-pinned MCP authorization: the KB pin and mcp scope survive
            // through the code into the (eventual) token. This is the slice's
            // chokepoint case — the default issued by consent is account
            // scope, but a db-pinned code is honored.
            let code = insert_oauth_code(
                &control,
                NewOAuthCode {
                    account_id: ACCOUNT_A,
                    client_id: &client_id,
                    code_challenge: "ch",
                    code_challenge_method: "S256",
                    redirect_uri: "http://localhost:3000/callback",
                    scope: TokenScope::Mcp,
                    allowed_db_id: Some("work-kb"),
                },
                OAUTH_CODE_TTL,
            )
            .await
            .expect("insert oauth code");

            let consumed = consume_oauth_code(&control, ACCOUNT_A, &code)
                .await
                .expect("consume query")
                .expect("live code consumes");
            assert_eq!(consumed.scope, TokenScope::Mcp);
            assert_eq!(consumed.allowed_db_id.as_deref(), Some("work-kb"));

            // The forensic link from spent code to issued token is recorded,
            // account-scoped.
            let stamped =
                record_oauth_code_token(&control, ACCOUNT_A, &consumed.code_hash, "tok-12345")
                    .await
                    .expect("record token id");
            assert_eq!(stamped, 1, "the consumed code is stamped with its token");

            // Account B cannot stamp account A's code.
            let cross = record_oauth_code_token(&control, ACCOUNT_B, &consumed.code_hash, "x")
                .await
                .expect("record token id");
            assert_eq!(cross, 0, "stamping is account-scoped");

            let token_id: Option<String> =
                sqlx::query_scalar("SELECT token_id FROM oauth_codes WHERE code_hash = $1")
                    .bind(&consumed.code_hash)
                    .fetch_one(control.pool())
                    .await
                    .expect("read token id");
            assert_eq!(token_id.as_deref(), Some("tok-12345"));
        },
    )
    .await;
}

#[tokio::test]
async fn client_secret_and_code_are_stored_hash_only() {
    with_control_db(
        "client_secret_and_code_are_stored_hash_only",
        |url| async move {
            let control = setup(&url).await;
            let (client_id, secret) =
                create_oauth_client(&control, ACCOUNT_A, "Claude Desktop", &[])
                    .await
                    .expect("register client");
            let code = issue_account_code(&control, ACCOUNT_A, &client_id, "ch").await;

            // The public client_id is intentionally stored verbatim.
            assert!(
                control_db_contains(&url, &client_id).await,
                "the public client_id is stored as-is"
            );

            // The secret plaintext and the code plaintext are NEVER persisted
            // anywhere in the control database — only their hashes are.
            assert!(
                !control_db_contains(&url, &secret).await,
                "the client secret plaintext must not be persisted"
            );
            assert!(
                !control_db_contains(&url, &code).await,
                "the authorization code plaintext must not be persisted"
            );
            // The prefixes alone must not leak through some prefix-only column.
            assert!(
                !control_db_contains(&url, OAUTH_CODE_PREFIX).await,
                "no oac_ plaintext prefix appears in the control database"
            );
        },
    )
    .await;
}

#[tokio::test]
async fn account_deletion_cascades_oauth_rows() {
    with_control_db("account_deletion_cascades_oauth_rows", |url| async move {
        let control = setup(&url).await;
        let (client_id, _) = create_oauth_client(&control, ACCOUNT_A, "Claude Desktop", &[])
            .await
            .expect("register client");
        let _code = issue_account_code(&control, ACCOUNT_A, &client_id, "ch").await;

        // Account B's rows survive A's deletion — proving the cascade is
        // account-scoped, not a blanket wipe.
        let (b_client_id, _) = create_oauth_client(&control, ACCOUNT_B, "Other", &[])
            .await
            .expect("register B client");
        let _b_code = issue_account_code(&control, ACCOUNT_B, &b_client_id, "ch").await;

        sqlx::query("DELETE FROM accounts WHERE id = $1")
            .bind(ACCOUNT_A)
            .execute(control.pool())
            .await
            .expect("delete account A");

        let a_clients: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM oauth_clients WHERE account_id = $1")
                .bind(ACCOUNT_A)
                .fetch_one(control.pool())
                .await
                .expect("count A clients");
        let a_codes: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM oauth_codes WHERE account_id = $1")
                .bind(ACCOUNT_A)
                .fetch_one(control.pool())
                .await
                .expect("count A codes");
        assert_eq!(a_clients, 0, "ON DELETE CASCADE removes A's oauth clients");
        assert_eq!(a_codes, 0, "ON DELETE CASCADE removes A's oauth codes");

        let b_clients: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM oauth_clients WHERE account_id = $1")
                .bind(ACCOUNT_B)
                .fetch_one(control.pool())
                .await
                .expect("count B clients");
        assert_eq!(b_clients, 1, "B's rows are untouched");
    })
    .await;
}
