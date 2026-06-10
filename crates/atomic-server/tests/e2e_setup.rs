//! End-to-end tests for the instance-setup flow.
//!
//! Setup endpoints (`/api/setup/status`, `/api/setup/claim`) are public:
//! they fire before the bearer-auth gate so a fresh instance can mint its
//! first token. The state machine has three observable values:
//!
//! - **No tokens, no `setup.claimed_at`** → status reports `needs_setup`,
//!   claim succeeds and mints a token.
//! - **Already claimed** → status reports `already_claimed`; subsequent
//!   claim returns 409.
//! - **Rate limit hit** → claim returns 429 once the IP-keyed bucket fills.
//!
//! All three rely on a freshly built `TestCtx` with
//! `mint_initial_token: false` so the seeded e2e token doesn't preempt
//! the unclaimed state.

mod support;

use serde_json::{json, Value};
use std::time::Duration;
use support::{spawn_live_server_with_public_routes, Backend, TestCtx, TestCtxOptions};

async fn boot(backend: Backend) -> Option<(TestCtx, support::LiveServer)> {
    let opts = TestCtxOptions {
        mint_initial_token: false,
        dangerously_skip_setup_token: true,
        ..Default::default()
    };
    let ctx = TestCtx::new_with(backend, opts).await?;
    let server = spawn_live_server_with_public_routes(&ctx).await;
    Some((ctx, server))
}

// ==================== SU1. Status on fresh instance ====================

#[actix_web::test]
async fn setup_status_reports_needs_setup_sqlite() {
    run_setup_status_reports_needs_setup(Backend::Sqlite).await;
}

#[actix_web::test]
async fn setup_status_reports_needs_setup_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("setup_status_reports_needs_setup_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_setup_status_reports_needs_setup(Backend::Postgres).await;
}

async fn run_setup_status_reports_needs_setup(backend: Backend) {
    let Some((_ctx, server)) = boot(backend).await else {
        return;
    };
    let client = reqwest::Client::new();
    let body: Value = client
        .get(format!("{}/api/setup/status", server.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["needs_setup"], true);
    assert_eq!(body["already_claimed"], false);

    server.stop().await;
}

// ==================== SU2. Claim then conflict ====================

#[actix_web::test]
async fn claim_succeeds_then_conflicts_sqlite() {
    run_claim_succeeds_then_conflicts(Backend::Sqlite).await;
}

#[actix_web::test]
async fn claim_succeeds_then_conflicts_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("claim_succeeds_then_conflicts_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_claim_succeeds_then_conflicts(Backend::Postgres).await;
}

async fn run_claim_succeeds_then_conflicts(backend: Backend) {
    let Some((_ctx, server)) = boot(backend).await else {
        return;
    };
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/api/setup/claim", server.base_url))
        .json(&json!({ "name": "first" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "first claim must succeed");
    let body: Value = resp.json().await.unwrap();
    let raw = body["token"].as_str().expect("token returned");
    assert!(!raw.is_empty());

    // Second claim must 409 — instance is now claimed.
    let resp = client
        .post(format!("{}/api/setup/claim", server.base_url))
        .json(&json!({ "name": "second" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        409,
        "subsequent claim on a claimed instance must be 409"
    );

    // Status now reports already_claimed.
    let body: Value = client
        .get(format!("{}/api/setup/status", server.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["already_claimed"], true);
    assert_eq!(body["needs_setup"], false);

    // The freshly minted token must authenticate.
    let resp = client
        .get(format!("{}/api/atoms", server.base_url))
        .bearer_auth(raw)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    server.stop().await;
}

// ==================== SU3. Rate limit kicks in ====================

#[actix_web::test]
async fn claim_rate_limited_after_burst_sqlite() {
    run_claim_rate_limited_after_burst(Backend::Sqlite).await;
}

#[actix_web::test]
async fn claim_rate_limited_after_burst_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("claim_rate_limited_after_burst_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_claim_rate_limited_after_burst(Backend::Postgres).await;
}

async fn run_claim_rate_limited_after_burst(backend: Backend) {
    let Some((_ctx, server)) = boot(backend).await else {
        return;
    };
    // The limiter is keyed on peer IP. All requests come from 127.0.0.1 so
    // they share a bucket of `SETUP_CLAIM_LIMIT = 10`. The first request
    // claims and exhausts subsequent requests via the 409 path, but each
    // attempt still consumes a bucket slot. After 10 attempts the limiter
    // returns 429 regardless of state.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    let mut saw_429 = false;
    for i in 0..15 {
        let resp = client
            .post(format!("{}/api/setup/claim", server.base_url))
            .json(&json!({ "name": format!("attempt-{i}") }))
            .send()
            .await
            .unwrap();
        if resp.status().as_u16() == 429 {
            saw_429 = true;
            break;
        }
    }
    assert!(
        saw_429,
        "expected a 429 within the bucket size (10 requests in 60s)"
    );

    server.stop().await;
}
