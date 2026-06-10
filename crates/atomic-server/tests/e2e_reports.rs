//! End-to-end tests for the reports surface.
//!
//! Reports are Atomic's autonomous-researcher primitive: a stored prompt +
//! source scope runs on a schedule (or manual trigger) and writes a
//! finding atom plus a citation row per `[N]` marker in the agent's
//! output. This suite covers the CRUD + manual-run lifecycle through the
//! HTTP boundary.
//!
//! Mock provider behavior (set up in `atomic_test_support::mock_ai`):
//!
//! - Research turn (non-streaming + tools) → immediately emits a
//!   `tool_calls` choice for `done`, ending research without running
//!   `semantic_search` / `read_atom`. Those tools are exercised by the
//!   chat e2e suite already.
//! - Final pass (non-streaming + `report_generation_result` schema) →
//!   returns markdown with a `[1]` marker plus `citations_used: [1]`.
//!   The runner's citation extractor maps the marker to the first source
//!   atom, producing a `report_finding_citations` row.

mod support;

use actix_web::test as actix_test;
use serde_json::{json, Value};
use std::time::Duration;
use support::{poll_until_embedding_done, test_app, Backend, TestCtx};

// ==================== Helpers ====================

async fn create_tag<S, B>(app: &S, auth: (&'static str, String), name: &str) -> String
where
    S: actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse<B>,
        Error = actix_web::Error,
    >,
    B: actix_web::body::MessageBody,
{
    let req = actix_test::TestRequest::post()
        .uri("/api/tags")
        .insert_header(auth)
        .set_json(json!({ "name": name, "parent_id": null }))
        .to_request();
    let resp = actix_test::call_service(app, req).await;
    assert_eq!(resp.status(), 201);
    let body: Value = actix_test::read_body_json(resp).await;
    body["id"].as_str().unwrap().to_string()
}

async fn seed_atom<S, B>(
    app: &S,
    auth: (&'static str, String),
    content: &str,
    tag_ids: &[&str],
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
        .set_json(json!({ "content": content, "tag_ids": tag_ids }))
        .to_request();
    let resp = actix_test::call_service(app, req).await;
    assert_eq!(resp.status(), 201);
    let body: Value = actix_test::read_body_json(resp).await;
    let id = body["id"].as_str().unwrap().to_string();
    poll_until_embedding_done(app, auth, &id).await;
    id
}

/// Build a minimal `CreateReportRequest` JSON payload. The handler
/// validates the cron expression, so we use a well-formed 6-field spec
/// even though the test triggers runs manually.
fn report_payload(name: &str, scope_tag: Option<&str>) -> Value {
    json!({
        "name": name,
        "description": null,
        "research_prompt": "Summarize what's known.",
        "source_scope_tag_ids": scope_tag.map(|t| vec![t]).unwrap_or_default(),
        "schedule": "0 0 * * * *",
        "enabled": true,
        "output_atom_tags": [],
    })
}

async fn create_report<S, B>(
    app: &S,
    auth: (&'static str, String),
    payload: Value,
) -> Value
where
    S: actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse<B>,
        Error = actix_web::Error,
    >,
    B: actix_web::body::MessageBody,
{
    let req = actix_test::TestRequest::post()
        .uri("/api/reports")
        .insert_header(auth)
        .set_json(payload)
        .to_request();
    let resp = actix_test::call_service(app, req).await;
    assert!(
        resp.status().is_success(),
        "POST /api/reports must succeed, got {}",
        resp.status()
    );
    actix_test::read_body_json(resp).await
}

// ==================== R1. Create round-trip ====================

#[actix_web::test]
async fn create_report_round_trip_sqlite() {
    run_create_report_round_trip(Backend::Sqlite).await;
}

#[actix_web::test]
async fn create_report_round_trip_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("create_report_round_trip_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_create_report_round_trip(Backend::Postgres).await;
}

async fn run_create_report_round_trip(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let report = create_report(&app, ctx.auth_header(), report_payload("Weekly Roundup", None)).await;
    let id = report["id"].as_str().expect("report id").to_string();
    assert_eq!(report["name"], "Weekly Roundup");
    assert_eq!(report["enabled"], true);

    let req = actix_test::TestRequest::get()
        .uri(&format!("/api/reports/{id}"))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let fetched: Value = actix_test::read_body_json(resp).await;
    assert_eq!(fetched["id"], id);
    assert_eq!(fetched["research_prompt"], "Summarize what's known.");
}

// ==================== R2. List ====================

#[actix_web::test]
async fn list_reports_returns_created_sqlite() {
    run_list_reports_returns_created(Backend::Sqlite).await;
}

#[actix_web::test]
async fn list_reports_returns_created_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("list_reports_returns_created_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_list_reports_returns_created(Backend::Postgres).await;
}

async fn run_list_reports_returns_created(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    for name in ["Alpha", "Beta"] {
        create_report(&app, ctx.auth_header(), report_payload(name, None)).await;
    }

    let req = actix_test::TestRequest::get()
        .uri("/api/reports")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let list: Vec<Value> = actix_test::read_body_json(resp).await;
    let names: Vec<&str> = list.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(names.contains(&"Alpha") && names.contains(&"Beta"));
}

// ==================== R3. Update ====================

#[actix_web::test]
async fn update_report_changes_fields_sqlite() {
    run_update_report_changes_fields(Backend::Sqlite).await;
}

#[actix_web::test]
async fn update_report_changes_fields_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("update_report_changes_fields_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_update_report_changes_fields(Backend::Postgres).await;
}

async fn run_update_report_changes_fields(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    let report = create_report(&app, ctx.auth_header(), report_payload("Initial", None)).await;
    let id = report["id"].as_str().unwrap().to_string();

    let req = actix_test::TestRequest::put()
        .uri(&format!("/api/reports/{id}"))
        .insert_header(ctx.auth_header())
        .set_json(json!({
            "name": "Renamed",
            "research_prompt": "Different prompt",
        }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "PUT must succeed");

    let req = actix_test::TestRequest::get()
        .uri(&format!("/api/reports/{id}"))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let fetched: Value = actix_test::read_body_json(resp).await;
    assert_eq!(fetched["name"], "Renamed");
    assert_eq!(fetched["research_prompt"], "Different prompt");
}

// ==================== R4. Toggle enabled ====================

#[actix_web::test]
async fn toggle_report_enabled_sqlite() {
    run_toggle_report_enabled(Backend::Sqlite).await;
}

#[actix_web::test]
async fn toggle_report_enabled_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("toggle_report_enabled_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_toggle_report_enabled(Backend::Postgres).await;
}

async fn run_toggle_report_enabled(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    let report = create_report(&app, ctx.auth_header(), report_payload("Pausable", None)).await;
    let id = report["id"].as_str().unwrap().to_string();
    assert_eq!(report["enabled"], true);

    let req = actix_test::TestRequest::patch()
        .uri(&format!("/api/reports/{id}/enabled"))
        .insert_header(ctx.auth_header())
        .set_json(json!({ "enabled": false }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 204);

    let req = actix_test::TestRequest::get()
        .uri(&format!("/api/reports/{id}"))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let fetched: Value = actix_test::read_body_json(resp).await;
    assert_eq!(fetched["enabled"], false);
}

// ==================== R5. Manual run writes finding ====================

#[actix_web::test]
async fn manual_run_writes_finding_atom_sqlite() {
    run_manual_run_writes_finding_atom(Backend::Sqlite).await;
}

#[actix_web::test]
async fn manual_run_writes_finding_atom_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("manual_run_writes_finding_atom_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_manual_run_writes_finding_atom(Backend::Postgres).await;
}

async fn run_manual_run_writes_finding_atom(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Seed two source atoms tagged "ReportScope" so the scope resolver
    // finds something — empty scope would short-circuit before the agent
    // and we'd test less.
    let scope_tag = create_tag(&app, ctx.auth_header(), "ReportScope").await;
    seed_atom(
        &app,
        ctx.auth_header(),
        "Subject under research; foundational note.",
        &[scope_tag.as_str()],
    )
    .await;
    seed_atom(
        &app,
        ctx.auth_header(),
        "Subject under research; follow-up insight.",
        &[scope_tag.as_str()],
    )
    .await;

    let report = create_report(
        &app,
        ctx.auth_header(),
        report_payload("RunMe", Some(scope_tag.as_str())),
    )
    .await;
    let id = report["id"].as_str().unwrap().to_string();

    let req = actix_test::TestRequest::post()
        .uri(&format!("/api/reports/{id}/run"))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 202, "manual run is async (202)");
    let dispatch: Value = actix_test::read_body_json(resp).await;
    assert_eq!(dispatch["status"], "dispatched");

    // Poll findings until non-empty. The runner is async on the same
    // tokio runtime; 15s is generous given the mock provider is in-proc.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let findings = loop {
        let req = actix_test::TestRequest::get()
            .uri(&format!("/api/reports/{id}/findings"))
            .insert_header(ctx.auth_header())
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body: Vec<Value> = actix_test::read_body_json(resp).await;
        if !body.is_empty() {
            break body;
        }
        if std::time::Instant::now() >= deadline {
            panic!("report run produced no findings within 15s");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // `list_findings_for_report` returns `Vec<(ReportFinding, AtomWithTags)>`
    // — a tuple serializes as a two-element array.
    assert_eq!(findings.len(), 1, "exactly one finding from one run");
    let finding_row = &findings[0][0];
    let atom_row = &findings[0][1];
    assert_eq!(finding_row["report_id"], id);
    let atom_id = atom_row["id"].as_str().expect("finding atom id").to_string();

    // Citations should resolve to one of the source atoms (the mock cites
    // marker [1] which the runner maps to the first source citable).
    let req = actix_test::TestRequest::get()
        .uri(&format!("/api/findings/{atom_id}/citations"))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let citations: Vec<Value> = actix_test::read_body_json(resp).await;
    assert!(!citations.is_empty(), "finding must have at least one citation row");
}

// ==================== R6. Delete ====================

#[actix_web::test]
async fn delete_report_round_trip_sqlite() {
    run_delete_report_round_trip(Backend::Sqlite).await;
}

#[actix_web::test]
async fn delete_report_round_trip_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("delete_report_round_trip_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_delete_report_round_trip(Backend::Postgres).await;
}

async fn run_delete_report_round_trip(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    let report = create_report(&app, ctx.auth_header(), report_payload("Deletable", None)).await;
    let id = report["id"].as_str().unwrap().to_string();

    let req = actix_test::TestRequest::delete()
        .uri(&format!("/api/reports/{id}"))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 204);

    let req = actix_test::TestRequest::get()
        .uri(&format!("/api/reports/{id}"))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404, "deleted report must 404");
}

// ==================== R7. Auth required ====================

#[actix_web::test]
async fn reports_require_auth_sqlite() {
    run_reports_require_auth(Backend::Sqlite).await;
}

#[actix_web::test]
async fn reports_require_auth_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("reports_require_auth_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_reports_require_auth(Backend::Postgres).await;
}

async fn run_reports_require_auth(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::get()
        .uri("/api/reports")
        .to_request();
    let resp = actix_test::try_call_service(&app, req).await;
    assert!(resp.is_err(), "unauthenticated reports list must be rejected");
}
