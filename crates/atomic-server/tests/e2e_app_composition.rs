//! End-to-end coverage for the composed application route table.
//!
//! `support::test_app` builds its `App` from
//! `atomic_server::app::configure_app` — the same function `main.rs` uses —
//! so this suite pins the full composition in one place: a public route
//! (`/health`) answers without auth, the bearer-gated `/api` scope accepts
//! a valid token and rejects a missing one, and the `/mcp` scope sits
//! behind `McpAuth` (401 with `WWW-Authenticate` when unauthenticated).
//! Every other e2e suite exercises the same wiring implicitly; this one
//! asserts the cross-scope layout directly so a regression in the
//! composition itself (e.g. a route slipping inside the wrong auth scope)
//! fails loudly rather than as a confusing downstream test error.

mod support;

use actix_web::test as actix_test;
use serde_json::Value;
use support::{test_app, Backend, TestCtx};

#[actix_web::test]
async fn full_composition_sqlite() {
    run_full_composition(Backend::Sqlite).await;
}

#[actix_web::test]
async fn full_composition_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("full_composition_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_full_composition(Backend::Postgres).await;
}

async fn run_full_composition(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Public route: /health answers without any credentials.
    let req = actix_test::TestRequest::get().uri("/health").to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "/health must be public");
    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));

    // Authenticated route: the /api scope admits a valid bearer token...
    let req = actix_test::TestRequest::get()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "/api must work with a valid token");

    // ...and rejects the same request without one. BearerAuth surfaces the
    // rejection as an actix error (rendered as a 401 over HTTP), so probe
    // the error's status rather than a ServiceResponse.
    let req = actix_test::TestRequest::get()
        .uri("/api/atoms")
        .to_request();
    let err = match actix_test::try_call_service(&app, req).await {
        Ok(resp) => panic!("/api must reject missing tokens, got {}", resp.status()),
        Err(err) => err,
    };
    assert_eq!(
        err.as_response_error().error_response().status(),
        401,
        "/api rejection must render as 401"
    );

    // MCP scope: McpAuth gates /mcp and advertises OAuth discovery on 401.
    let req = actix_test::TestRequest::post().uri("/mcp").to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401, "/mcp must reject missing tokens");
    let www_authenticate = resp
        .headers()
        .get("WWW-Authenticate")
        .and_then(|v| v.to_str().ok())
        .expect("MCP 401 must carry WWW-Authenticate");
    assert!(
        www_authenticate.contains("resource_metadata="),
        "MCP 401 must point at OAuth discovery, got: {www_authenticate}"
    );
}
