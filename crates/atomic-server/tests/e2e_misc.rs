//! End-to-end tests for the remaining HTTP surface — canvas, clustering,
//! graph, embedding maintenance, dashboard, logs, utils, and database
//! management.
//!
//! These routes are individually small but together cover the long tail of
//! the API. The point is contract pinning: each test exercises one route's
//! happy path so a regression in storage routing or response shape
//! surfaces in CI.
//!
//! The Ollama discovery endpoints are not exercised here because they hit a
//! real Ollama server. Hooking them up would require a separate mock that
//! speaks Ollama's `/api/tags` shape; defer until we ship Ollama-on-cloud.

mod support;

use actix_web::test as actix_test;
use serde_json::{json, Value};
use std::time::Duration;
use support::{poll_until_embedding_done, test_app, Backend, TestCtx};

// ==================== Helpers ====================

async fn seed_atom<S, B>(
    app: &S,
    auth: (&'static str, String),
    content: &str,
) -> String
where
    S: actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse<B>,
        Error = actix_web::Error,
    >,
    B: actix_web::body::MessageBody,
{
    let req = actix_test::TestRequest::post()
        .uri("/api/atoms")
        .insert_header(auth.clone())
        .set_json(json!({ "content": content }))
        .to_request();
    let resp = actix_test::call_service(app, req).await;
    assert_eq!(resp.status(), 201);
    let body: Value = actix_test::read_body_json(resp).await;
    let id = body["id"].as_str().unwrap().to_string();
    poll_until_embedding_done(app, auth, &id).await;
    id
}

// ==================== Canvas ====================

#[actix_web::test]
async fn canvas_positions_round_trip_sqlite() {
    run_canvas_positions_round_trip(Backend::Sqlite).await;
}

#[actix_web::test]
async fn canvas_positions_round_trip_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("canvas_positions_round_trip_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_canvas_positions_round_trip(Backend::Postgres).await;
}

async fn run_canvas_positions_round_trip(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    let id = seed_atom(&app, ctx.auth_header(), "position me on the canvas").await;

    let req = actix_test::TestRequest::put()
        .uri("/api/canvas/positions")
        .insert_header(ctx.auth_header())
        .set_json(json!([{ "atom_id": id, "x": 12.5, "y": -7.25 }]))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let req = actix_test::TestRequest::get()
        .uri("/api/canvas/positions")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let positions: Vec<Value> = actix_test::read_body_json(resp).await;
    let saved = positions
        .iter()
        .find(|p| p["atom_id"] == id)
        .expect("position persisted");
    assert!((saved["x"].as_f64().unwrap() - 12.5).abs() < 1e-6);
    assert!((saved["y"].as_f64().unwrap() - -7.25).abs() < 1e-6);
}

// ==================== Clustering ====================

#[actix_web::test]
async fn clustering_get_returns_empty_initially_sqlite() {
    run_clustering_get_returns_empty_initially(Backend::Sqlite).await;
}

#[actix_web::test]
async fn clustering_get_returns_empty_initially_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("clustering_get_returns_empty_initially_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_clustering_get_returns_empty_initially(Backend::Postgres).await;
}

async fn run_clustering_get_returns_empty_initially(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    // Fresh DB: no atoms, no clusters. The route must still respond 200.
    let req = actix_test::TestRequest::get()
        .uri("/api/clustering")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body: Vec<Value> = actix_test::read_body_json(resp).await;
    assert!(body.is_empty(), "fresh DB must yield zero clusters");
}

// ==================== Graph ====================

#[actix_web::test]
async fn graph_edges_after_pipeline_sqlite() {
    run_graph_edges_after_pipeline(Backend::Sqlite).await;
}

#[actix_web::test]
async fn graph_edges_after_pipeline_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("graph_edges_after_pipeline_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_graph_edges_after_pipeline(Backend::Postgres).await;
}

async fn run_graph_edges_after_pipeline(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Two related atoms (overlapping vocabulary) produce an edge above
    // 0.5 with the bag-of-words mock embedder. Edge computation is the
    // last step of the per-atom pipeline; poll until the edges table
    // settles rather than racing immediately after the per-atom poller
    // returns.
    seed_atom(&app, ctx.auth_header(), "quantum particles waves momentum").await;
    seed_atom(&app, ctx.auth_header(), "quantum particles momentum").await;

    // Force a synchronous edge rebuild — the per-atom pipeline writes
    // edges as part of embedding completion, but the timing is racy in
    // tests that need to read them back immediately. `rebuild-edges`
    // returns the queued count and the work runs on the same loop.
    let req = actix_test::TestRequest::post()
        .uri("/api/graph/rebuild-edges")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let req = actix_test::TestRequest::get()
            .uri("/api/graph/edges?min_similarity=0.0")
            .insert_header(ctx.auth_header())
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let edges: Vec<Value> = actix_test::read_body_json(resp).await;
        if !edges.is_empty() {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("shared-vocabulary atoms produced no edge within 15s after rebuild");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ==================== Embedding maintenance ====================

#[actix_web::test]
async fn reembed_all_increases_embedding_hits_sqlite() {
    run_reembed_all_increases_embedding_hits(Backend::Sqlite).await;
}

#[actix_web::test]
async fn reembed_all_increases_embedding_hits_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("reembed_all_increases_embedding_hits_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_reembed_all_increases_embedding_hits(Backend::Postgres).await;
}

async fn run_reembed_all_increases_embedding_hits(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    seed_atom(&app, ctx.auth_header(), "reembed me later").await;
    ctx.mock.reset_counts();

    let req = actix_test::TestRequest::post()
        .uri("/api/embeddings/reembed-all")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // The reembed runs on a background task; bound the wait and assert
    // the mock saw the second pass.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while ctx.mock.embedding_request_count() == 0 {
        if std::time::Instant::now() >= deadline {
            panic!("reembed-all did not hit the embedding endpoint within 15s");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// ==================== Dashboard ====================

#[actix_web::test]
async fn dashboard_featured_report_round_trip_sqlite() {
    run_dashboard_featured_report_round_trip(Backend::Sqlite).await;
}

#[actix_web::test]
async fn dashboard_featured_report_round_trip_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("dashboard_featured_report_round_trip_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_dashboard_featured_report_round_trip(Backend::Postgres).await;
}

async fn run_dashboard_featured_report_round_trip(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Need a real report id to feature — the route validates the
    // referenced report exists.
    let req = actix_test::TestRequest::post()
        .uri("/api/reports")
        .insert_header(ctx.auth_header())
        .set_json(json!({
            "name": "Featurable",
            "description": null,
            "research_prompt": "p",
            "schedule": "0 0 * * * *",
            "enabled": true,
            "source_scope_tag_ids": [],
            "output_atom_tags": [],
        }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let body: Value = actix_test::read_body_json(resp).await;
    let report_id = body["id"].as_str().unwrap().to_string();

    let req = actix_test::TestRequest::put()
        .uri("/api/dashboard/featured-report")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "report_id": report_id }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let req = actix_test::TestRequest::get()
        .uri("/api/dashboard/featured-report")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["report_id"], report_id);
}

// ==================== Logs ====================

#[actix_web::test]
async fn logs_endpoint_returns_string_sqlite() {
    run_logs_endpoint_returns_string(Backend::Sqlite).await;
}

#[actix_web::test]
async fn logs_endpoint_returns_string_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("logs_endpoint_returns_string_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_logs_endpoint_returns_string(Backend::Postgres).await;
}

async fn run_logs_endpoint_returns_string(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::get()
        .uri("/api/logs")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body: Value = actix_test::read_body_json(resp).await;
    assert!(body["logs"].is_string(), "logs field must be a string");
}

// ==================== Databases ====================

#[actix_web::test]
async fn create_then_delete_database_sqlite() {
    run_create_then_delete_database(Backend::Sqlite).await;
}

#[actix_web::test]
async fn create_then_delete_database_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("create_then_delete_database_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_create_then_delete_database(Backend::Postgres).await;
}

async fn run_create_then_delete_database(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/databases")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "name": "extra-db" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);
    let body: Value = actix_test::read_body_json(resp).await;
    let id = body["id"].as_str().unwrap().to_string();

    let req = actix_test::TestRequest::get()
        .uri("/api/databases")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let body: Value = actix_test::read_body_json(resp).await;
    let dbs: Vec<Value> = body["databases"].as_array().cloned().unwrap_or_default();
    assert!(dbs.iter().any(|d| d["id"] == id), "new DB must appear in list");

    let req = actix_test::TestRequest::delete()
        .uri(&format!("/api/databases/{id}"))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "delete must succeed; got {}", resp.status());

    let req = actix_test::TestRequest::get()
        .uri("/api/databases")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let body: Value = actix_test::read_body_json(resp).await;
    let dbs: Vec<Value> = body["databases"].as_array().cloned().unwrap_or_default();
    assert!(
        dbs.iter().all(|d| d["id"] != id),
        "deleted DB must not appear in subsequent list"
    );
}

#[actix_web::test]
async fn rename_database_round_trip_sqlite() {
    run_rename_database_round_trip(Backend::Sqlite).await;
}

#[actix_web::test]
async fn rename_database_round_trip_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("rename_database_round_trip_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_rename_database_round_trip(Backend::Postgres).await;
}

async fn run_rename_database_round_trip(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/databases")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "name": "before-rename" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let body: Value = actix_test::read_body_json(resp).await;
    let id = body["id"].as_str().unwrap().to_string();

    let req = actix_test::TestRequest::put()
        .uri(&format!("/api/databases/{id}"))
        .insert_header(ctx.auth_header())
        .set_json(json!({ "name": "after-rename" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let req = actix_test::TestRequest::get()
        .uri("/api/databases")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let body: Value = actix_test::read_body_json(resp).await;
    let dbs: Vec<Value> = body["databases"].as_array().cloned().unwrap_or_default();
    let entry = dbs
        .iter()
        .find(|d| d["id"] == id)
        .expect("renamed DB still listed");
    assert_eq!(entry["name"], "after-rename");
}

// ==================== Utils ====================

#[actix_web::test]
async fn utils_sqlite_vec_returns_version_sqlite() {
    run_utils_sqlite_vec_returns_version(Backend::Sqlite).await;
}

// (Postgres has no sqlite-vec; the route is SQLite-only so we deliberately
// skip the PG arm rather than asserting on backend-specific behavior.)

async fn run_utils_sqlite_vec_returns_version(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::get()
        .uri("/api/utils/sqlite-vec")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "sqlite-vec must be loaded in tests");
    let body: Value = actix_test::read_body_json(resp).await;
    assert!(body["version"].is_string());
}
