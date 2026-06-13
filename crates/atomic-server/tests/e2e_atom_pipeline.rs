//! End-to-end atom CRUD + pipeline tests across both storage backends.
//!
//! Each test creates an atom over HTTP, polls the GET endpoint until the
//! pipeline reports a terminal state, then asserts on the persisted shape:
//! embedding completed, an auto-tag attached, and the original content
//! round-trips. The mock AI provider (in `support::MockAiServer`) responds
//! instantly so the budget for the pipeline is bounded by tokio scheduler
//! latency, not network.

mod support;

use actix_web::test as actix_test;
use serde_json::{json, Value};
use support::{poll_until_embedding_done, test_app, Backend, TestCtx};

// ==================== Create → poll → done ====================

#[actix_web::test]
async fn create_atom_runs_full_pipeline_sqlite() {
    run_create_atom_runs_full_pipeline(Backend::Sqlite).await;
}

#[actix_web::test]
async fn create_atom_runs_full_pipeline_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "create_atom_runs_full_pipeline_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_create_atom_runs_full_pipeline(Backend::Postgres).await;
}

async fn run_create_atom_runs_full_pipeline(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .set_json(json!({
            "content": "quantum particles atomic waves momentum",
        }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201, "POST /api/atoms should return 201");

    let body: Value = actix_test::read_body_json(resp).await;
    let atom_id = body["id"].as_str().expect("response has id").to_string();
    assert_eq!(
        body["embedding_status"], "pending",
        "atom should start in pending state — pipeline runs in background"
    );

    // Pipeline runs on a background tokio task. Poll until it hits a terminal
    // state, then check the AI provider actually got hit.
    let final_body = poll_until_embedding_done(&app, ctx.auth_header(), &atom_id).await;
    assert_eq!(
        final_body["embedding_status"], "complete",
        "embedding should succeed: {final_body}"
    );
    assert_eq!(
        final_body["content"],
        "quantum particles atomic waves momentum"
    );

    assert!(
        ctx.mock.embedding_request_count() >= 1,
        "mock embedding endpoint should have been hit"
    );

    // Auto-tagging fires after embedding completes. `embedding_status =
    // complete` is reached before the tagger writes its rows, so the
    // tags-array check needs to wait on `tagging_status` separately.
    let final_body = support::poll_until_tagging_done(&app, ctx.auth_header(), &atom_id).await;
    let tags = final_body["tags"]
        .as_array()
        .expect("atom response has tags array")
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect::<Vec<_>>();
    assert!(
        tags.iter().any(|t| t == "Physics"),
        "expected Physics tag from auto-tagging; got {:?}",
        tags
    );
}

// ==================== List / pagination after create ====================

#[actix_web::test]
async fn list_atoms_returns_created_atoms_sqlite() {
    run_list_atoms_returns_created_atoms(Backend::Sqlite).await;
}

#[actix_web::test]
async fn list_atoms_returns_created_atoms_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "list_atoms_returns_created_atoms_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_list_atoms_returns_created_atoms(Backend::Postgres).await;
}

async fn run_list_atoms_returns_created_atoms(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    for i in 0..3 {
        let req = actix_test::TestRequest::post()
            .uri("/api/atoms")
            .insert_header(ctx.auth_header())
            .set_json(json!({ "content": format!("test atom {i}") }))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 201);
    }

    let req = actix_test::TestRequest::get()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["total_count"], 3, "list should report 3 atoms");
    assert_eq!(body["atoms"].as_array().unwrap().len(), 3);
}

// ==================== Delete ====================

#[actix_web::test]
async fn delete_atom_round_trip_sqlite() {
    run_delete_atom_round_trip(Backend::Sqlite).await;
}

#[actix_web::test]
async fn delete_atom_round_trip_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("delete_atom_round_trip_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_delete_atom_round_trip(Backend::Postgres).await;
}

async fn run_delete_atom_round_trip(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "content": "to be deleted" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);
    let body: Value = actix_test::read_body_json(resp).await;
    let atom_id = body["id"].as_str().unwrap().to_string();

    let req = actix_test::TestRequest::delete()
        .uri(&format!("/api/atoms/{atom_id}"))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "DELETE should succeed, got {}",
        resp.status()
    );

    let req = actix_test::TestRequest::get()
        .uri(&format!("/api/atoms/{atom_id}"))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        404,
        "deleted atom should 404 on subsequent GET"
    );
}
