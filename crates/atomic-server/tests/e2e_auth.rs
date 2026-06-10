//! End-to-end auth tests across both storage backends.
//!
//! `BearerAuth` middleware reads the token, looks it up via `core.verify_api_token`,
//! and either lets the request through or rejects with 401. The same code path
//! runs against either backend; this suite pins the contract on both.

mod support;

use actix_web::test as actix_test;
use support::{test_app, Backend, TestCtx};

// ==================== Valid token ====================

#[actix_web::test]
async fn valid_bearer_token_sqlite() {
    run_valid_bearer_token(Backend::Sqlite).await;
}

#[actix_web::test]
async fn valid_bearer_token_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("valid_bearer_token_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_valid_bearer_token(Backend::Postgres).await;
}

async fn run_valid_bearer_token(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::get()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "valid token must pass the auth gate");
}

// ==================== Missing token ====================

#[actix_web::test]
async fn missing_bearer_token_sqlite() {
    run_missing_bearer_token(Backend::Sqlite).await;
}

#[actix_web::test]
async fn missing_bearer_token_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("missing_bearer_token_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_missing_bearer_token(Backend::Postgres).await;
}

async fn run_missing_bearer_token(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::get()
        .uri("/api/atoms")
        .to_request();
    let resp = actix_test::try_call_service(&app, req).await;
    assert!(
        resp.is_err(),
        "request without Authorization header should be rejected"
    );
}

// ==================== Wrong token ====================

#[actix_web::test]
async fn wrong_bearer_token_sqlite() {
    run_wrong_bearer_token(Backend::Sqlite).await;
}

#[actix_web::test]
async fn wrong_bearer_token_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("wrong_bearer_token_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_wrong_bearer_token(Backend::Postgres).await;
}

async fn run_wrong_bearer_token(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::get()
        .uri("/api/atoms")
        .insert_header(("Authorization", "Bearer wrong-secret-token"))
        .to_request();
    let resp = actix_test::try_call_service(&app, req).await;
    assert!(resp.is_err(), "unknown token should be rejected");
}

// ==================== Revoked token ====================

#[actix_web::test]
async fn revoked_bearer_token_sqlite() {
    run_revoked_bearer_token(Backend::Sqlite).await;
}

#[actix_web::test]
async fn revoked_bearer_token_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("revoked_bearer_token_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_revoked_bearer_token(Backend::Postgres).await;
}

async fn run_revoked_bearer_token(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };

    // `revoke_api_token` refuses to clear the last active token to avoid
    // locking ourselves out of the API. Mint a replacement first, then revoke
    // the seeded one — the original token's row still exists but `revoked_at`
    // is now set, so the BearerAuth lookup must reject it.
    let core = ctx.state.manager.active_core().await.expect("active core");
    core.create_api_token("replacement")
        .await
        .expect("mint replacement token");
    let tokens = core.list_api_tokens().await.expect("list tokens");
    let token_id = &tokens
        .iter()
        .find(|t| t.name == "e2e-test")
        .expect("seeded e2e-test token")
        .id;
    core.revoke_api_token(token_id)
        .await
        .expect("revoke token");

    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::get()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::try_call_service(&app, req).await;
    assert!(resp.is_err(), "revoked token should be rejected");
}
