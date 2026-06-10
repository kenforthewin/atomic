//! Concurrent HTTP-load tests across both backends.
//!
//! Fires N parallel `POST /api/atoms` and verifies every request gets a 201
//! and every atom is queryable afterward. Two failure modes this catches:
//!
//!   1. Postgres pool exhaustion — the pre-hardening pool was hardcoded at
//!      50 connections with a 10s acquire timeout. A storm of in-flight
//!      requests + the pipeline's own pool checkouts could trip the timeout
//!      and surface as 500s.
//!   2. SQLite write contention — concurrent writers serialize through the
//!      busy-handler. We bound the storm small enough that the SQLite arm
//!      stays a sanity check rather than a stress test.

mod support;

use serde_json::{json, Value};
use std::collections::HashSet;
use support::{spawn_live_server, Backend, TestCtx};

const STORM_SIZE: usize = 20;

#[actix_web::test]
async fn concurrent_atom_creation_sqlite() {
    run_concurrent_atom_creation(Backend::Sqlite).await;
}

#[actix_web::test]
async fn concurrent_atom_creation_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "concurrent_atom_creation_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_concurrent_atom_creation(Backend::Postgres).await;
}

async fn run_concurrent_atom_creation(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let server = spawn_live_server(&ctx).await;
    let client = reqwest::Client::new();

    // Fire all requests at once with `join_all` so they overlap on the pool.
    let mut handles = Vec::with_capacity(STORM_SIZE);
    for i in 0..STORM_SIZE {
        let client = client.clone();
        let url = format!("{}/api/atoms", server.base_url);
        let token = ctx.token.clone();
        handles.push(tokio::spawn(async move {
            let resp = client
                .post(&url)
                .bearer_auth(&token)
                .json(&json!({ "content": format!("concurrent atom {i}") }))
                .send()
                .await
                .map_err(|e| format!("request {i} failed: {e}"))?;
            if resp.status() != 201 {
                return Err(format!(
                    "request {i} expected 201, got {} (body: {})",
                    resp.status(),
                    resp.text().await.unwrap_or_default()
                ));
            }
            let body: Value = resp.json().await.map_err(|e| format!("parse {i}: {e}"))?;
            Ok::<String, String>(body["id"].as_str().expect("id field").to_string())
        }));
    }

    let mut created_ids = HashSet::with_capacity(STORM_SIZE);
    for (i, handle) in handles.into_iter().enumerate() {
        let id = handle
            .await
            .expect("storm task panicked")
            .unwrap_or_else(|e| panic!("storm request {i}: {e}"));
        created_ids.insert(id);
    }
    assert_eq!(
        created_ids.len(),
        STORM_SIZE,
        "every concurrent request should produce a unique atom id"
    );

    // List endpoint should now report all of them. Read in one shot —
    // the page-size default is large enough for STORM_SIZE atoms.
    let resp = client
        .get(format!("{}/api/atoms?limit={}", server.base_url, STORM_SIZE))
        .bearer_auth(&ctx.token)
        .send()
        .await
        .expect("GET /api/atoms");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("parse list response");
    let listed: HashSet<String> = body["atoms"]
        .as_array()
        .expect("atoms array")
        .iter()
        .filter_map(|a| a["id"].as_str().map(str::to_string))
        .collect();
    assert!(
        created_ids.is_subset(&listed),
        "all created atoms should be visible in the list; missing = {:?}",
        created_ids.difference(&listed).collect::<Vec<_>>()
    );

    server.stop().await;
}
