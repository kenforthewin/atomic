//! End-to-end tests for the API-token management surface.
//!
//! Bearer-auth validation itself (good/bad/missing/revoked) is covered by
//! `e2e_auth.rs`. This file exercises the issuance and lifecycle endpoints
//! — `POST /api/auth/tokens`, `GET /api/auth/tokens`,
//! `DELETE /api/auth/tokens/{id}` — and the last-token-revocation
//! contract that keeps an instance from locking itself out via the API.

mod support;

use actix_web::test as actix_test;
use serde_json::{json, Value};
use support::{test_app, Backend, TestCtx};

// ==================== K1. Create round-trip ====================

#[actix_web::test]
async fn create_token_returns_raw_value_sqlite() {
    run_create_token_returns_raw_value(Backend::Sqlite).await;
}

#[actix_web::test]
async fn create_token_returns_raw_value_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "create_token_returns_raw_value_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_create_token_returns_raw_value(Backend::Postgres).await;
}

async fn run_create_token_returns_raw_value(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/auth/tokens")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "name": "my-laptop" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);
    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["name"], "my-laptop");
    let raw = body["token"].as_str().expect("token returned exactly once");
    assert!(!raw.is_empty(), "raw token must be non-empty");

    // The newly issued token must work against an authenticated endpoint.
    let req = actix_test::TestRequest::get()
        .uri("/api/atoms")
        .insert_header(("Authorization", format!("Bearer {raw}")))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "newly issued token must authenticate");
}

// ==================== K2. List omits raw values ====================

#[actix_web::test]
async fn list_tokens_returns_metadata_only_sqlite() {
    run_list_tokens_returns_metadata_only(Backend::Sqlite).await;
}

#[actix_web::test]
async fn list_tokens_returns_metadata_only_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("list_tokens_returns_metadata_only_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_list_tokens_returns_metadata_only(Backend::Postgres).await;
}

async fn run_list_tokens_returns_metadata_only(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Mint a second token so the list returns more than one row.
    let req = actix_test::TestRequest::post()
        .uri("/api/auth/tokens")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "name": "second-device" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);

    let req = actix_test::TestRequest::get()
        .uri("/api/auth/tokens")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let tokens: Vec<Value> = actix_test::read_body_json(resp).await;
    assert!(tokens.len() >= 2, "expected at least 2 tokens");
    for t in &tokens {
        assert!(
            t.get("token").is_none() && t.get("hash").is_none(),
            "list must not include raw token or hash; got {t}"
        );
    }
}

// ==================== K3. Revoked token rejected ====================

#[actix_web::test]
async fn revoked_token_rejected_on_subsequent_requests_sqlite() {
    run_revoked_token_rejected_on_subsequent_requests(Backend::Sqlite).await;
}

#[actix_web::test]
async fn revoked_token_rejected_on_subsequent_requests_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("revoked_token_rejected_on_subsequent_requests_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_revoked_token_rejected_on_subsequent_requests(Backend::Postgres).await;
}

async fn run_revoked_token_rejected_on_subsequent_requests(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Mint a fresh "victim" token that we'll revoke.
    let req = actix_test::TestRequest::post()
        .uri("/api/auth/tokens")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "name": "victim" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let body: Value = actix_test::read_body_json(resp).await;
    let victim_raw = body["token"].as_str().unwrap().to_string();
    let victim_id = body["id"].as_str().unwrap().to_string();

    // Revoke via the seeded e2e-test token's auth.
    let req = actix_test::TestRequest::delete()
        .uri(&format!("/api/auth/tokens/{victim_id}"))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // The revoked bearer must no longer authenticate.
    let req = actix_test::TestRequest::get()
        .uri("/api/atoms")
        .insert_header(("Authorization", format!("Bearer {victim_raw}")))
        .to_request();
    let resp = actix_test::try_call_service(&app, req).await;
    assert!(resp.is_err(), "revoked token must not pass BearerAuth");
}

// ==================== K4. Cannot revoke last token ====================

#[actix_web::test]
async fn cannot_revoke_last_token_sqlite() {
    run_cannot_revoke_last_token(Backend::Sqlite).await;
}

#[actix_web::test]
async fn cannot_revoke_last_token_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("cannot_revoke_last_token_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_cannot_revoke_last_token(Backend::Postgres).await;
}

async fn run_cannot_revoke_last_token(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Find the seeded e2e-test token id (the only active token).
    let req = actix_test::TestRequest::get()
        .uri("/api/auth/tokens")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let tokens: Vec<Value> = actix_test::read_body_json(resp).await;
    let only_id = tokens
        .iter()
        .find(|t| t["name"] == "e2e-test")
        .and_then(|t| t["id"].as_str())
        .expect("seeded e2e-test token")
        .to_string();

    let req = actix_test::TestRequest::delete()
        .uri(&format!("/api/auth/tokens/{only_id}"))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(
        !resp.status().is_success(),
        "revoking the only token must be rejected to avoid self-lockout"
    );

    // Token must still work — verifies the route didn't half-revoke.
    let req = actix_test::TestRequest::get()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "last token must remain valid");
}
