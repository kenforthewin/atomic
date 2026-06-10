//! End-to-end smoke test for the MCP HTTP endpoint.
//!
//! Validates the gap that mcp_auth.rs's unit tests can't reach: the McpAuth
//! middleware wrapped around the real AtomicMcpTransport scope, mounted in
//! a live actix server, reachable over HTTP. We exercise the auth gate and
//! a minimal protocol round-trip (`initialize`) so a regression in either
//! the auth wiring or the transport scope surfaces here.
//!
//! Deeper MCP protocol semantics (tool dispatch, session lifecycle,
//! cancellation) belong in the rmcp crate's own suite; this file owns the
//! "does our server expose MCP at all" contract.

mod support;

use serde_json::{json, Value};
use support::{spawn_live_server, Backend, TestCtx};

#[actix_web::test]
async fn mcp_rejects_missing_auth_sqlite() {
    run_mcp_rejects_missing_auth(Backend::Sqlite).await;
}

#[actix_web::test]
async fn mcp_rejects_missing_auth_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("mcp_rejects_missing_auth_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_mcp_rejects_missing_auth(Backend::Postgres).await;
}

async fn run_mcp_rejects_missing_auth(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let server = spawn_live_server(&ctx).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/mcp", server.base_url))
        .json(&initialize_request())
        .send()
        .await
        .expect("POST /mcp without auth");
    assert_eq!(resp.status(), 401, "missing Bearer should yield 401");
    let www_authenticate = resp
        .headers()
        .get("WWW-Authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        www_authenticate.starts_with("Bearer "),
        "WWW-Authenticate header should carry Bearer challenge; got {www_authenticate:?}"
    );

    server.stop().await;
}

#[actix_web::test]
async fn mcp_rejects_wrong_token_sqlite() {
    run_mcp_rejects_wrong_token(Backend::Sqlite).await;
}

#[actix_web::test]
async fn mcp_rejects_wrong_token_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("mcp_rejects_wrong_token_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_mcp_rejects_wrong_token(Backend::Postgres).await;
}

async fn run_mcp_rejects_wrong_token(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let server = spawn_live_server(&ctx).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/mcp", server.base_url))
        .bearer_auth("not-a-real-token")
        .json(&initialize_request())
        .send()
        .await
        .expect("POST /mcp with wrong token");
    assert_eq!(resp.status(), 401, "unknown token should be 401");

    server.stop().await;
}

#[actix_web::test]
async fn mcp_initialize_round_trip_sqlite() {
    run_mcp_initialize_round_trip(Backend::Sqlite).await;
}

#[actix_web::test]
async fn mcp_initialize_round_trip_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("mcp_initialize_round_trip_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_mcp_initialize_round_trip(Backend::Postgres).await;
}

async fn run_mcp_initialize_round_trip(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let server = spawn_live_server(&ctx).await;

    let client = reqwest::Client::new();
    // Streamable HTTP transport will pick JSON or SSE based on Accept. We
    // ask for both so the server can choose whichever shape it prefers for
    // `initialize` — both prove the route is reachable through McpAuth.
    let resp = client
        .post(format!("{}/mcp", server.base_url))
        .bearer_auth(&ctx.token)
        .header("Accept", "application/json, text/event-stream")
        .json(&initialize_request())
        .send()
        .await
        .expect("POST /mcp initialize");

    assert!(
        resp.status().is_success(),
        "MCP initialize should succeed; got {} (body: {})",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );

    // Streamable HTTP returns either application/json or text/event-stream.
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = resp.text().await.expect("read body");

    let result_present = if content_type.starts_with("application/json") {
        let parsed: Value = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("invalid JSON body: {e}\nbody = {body}"));
        parsed.get("result").is_some() || parsed.get("error").is_some()
    } else {
        // SSE framing: look for a `data:` line carrying a JSON-RPC payload.
        body.lines().any(|line| {
            line.strip_prefix("data: ").is_some_and(|payload| {
                serde_json::from_str::<Value>(payload)
                    .ok()
                    .and_then(|v| {
                        if v.get("result").is_some() || v.get("error").is_some() {
                            Some(())
                        } else {
                            None
                        }
                    })
                    .is_some()
            })
        })
    };
    assert!(
        result_present,
        "MCP initialize response should carry a JSON-RPC result or error; \
         content-type = {content_type:?}, body = {body}"
    );

    server.stop().await;
}

fn initialize_request() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "atomic-e2e", "version": "0.0.0" }
        }
    })
}
