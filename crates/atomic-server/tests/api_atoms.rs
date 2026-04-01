//! Integration tests for the Atom and Tag REST API endpoints.
//!
//! Each test spins up a real actix-web test server backed by a temporary SQLite
//! database and exercises the endpoints with actual HTTP requests.

use actix_web::{test as actix_test, web, App, ResponseError};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::broadcast;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Shared test context — holds the temp dir (so the DB stays alive),
/// the app state, and the raw auth token.
struct TestCtx {
    _temp: tempfile::TempDir,
    state: web::Data<atomic_server::state::AppState>,
    token: String,
}

impl TestCtx {
    fn new() -> Self {
        let temp = tempfile::TempDir::new().unwrap();
        let manager = Arc::new(
            atomic_core::DatabaseManager::new(temp.path()).unwrap(),
        );
        let (_info, raw_token) = manager.registry().unwrap().create_api_token("test").unwrap();
        let (event_tx, _) = broadcast::channel(16);
        let state = web::Data::new(atomic_server::state::AppState {
            manager,
            event_tx,
            public_url: None,
            log_buffer: atomic_server::log_buffer::LogBuffer::new(10),
        });
        TestCtx {
            _temp: temp,
            state,
            token: raw_token,
        }
    }

    fn auth_header(&self) -> (&str, String) {
        ("Authorization", format!("Bearer {}", self.token))
    }
}

/// Build an actix App that mirrors the real server's /api scope (auth + routes).
fn test_app(
    ctx: &TestCtx,
) -> App<
    impl actix_web::dev::ServiceFactory<
        actix_web::dev::ServiceRequest,
        Config = (),
        Response = actix_web::dev::ServiceResponse<impl actix_web::body::MessageBody>,
        Error = actix_web::Error,
        InitError = (),
    >,
> {
    App::new()
        .app_data(ctx.state.clone())
        .service(
            web::scope("/api")
                .wrap(atomic_server::auth::BearerAuth {
                    state: ctx.state.clone(),
                })
                .configure(atomic_server::routes::configure_routes),
        )
}

// ---------------------------------------------------------------------------
// Atom CRUD tests
// ---------------------------------------------------------------------------

#[actix_web::test]
async fn test_create_and_get_atom() {
    let ctx = TestCtx::new();
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Create
    let req = actix_test::TestRequest::post()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .set_json(json!({
            "content": "# Hello\n\nThis is a test atom.",
        }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201, "create should return 201");

    let body: Value = actix_test::read_body_json(resp).await;
    let atom_id = body["id"].as_str().expect("response should have id");
    assert_eq!(body["content"], "# Hello\n\nThis is a test atom.");
    assert!(body["tags"].as_array().unwrap().is_empty());

    // Get
    let req = actix_test::TestRequest::get()
        .uri(&format!("/api/atoms/{}", atom_id))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["id"], atom_id);
    assert_eq!(body["content"], "# Hello\n\nThis is a test atom.");
}

#[actix_web::test]
async fn test_get_atom_not_found() {
    let ctx = TestCtx::new();
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::get()
        .uri("/api/atoms/nonexistent-id")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}

#[actix_web::test]
async fn test_list_atoms_empty() {
    let ctx = TestCtx::new();
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::get()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["total_count"], 0);
    assert!(body["atoms"].as_array().unwrap().is_empty());
}

#[actix_web::test]
async fn test_list_atoms_with_pagination() {
    let ctx = TestCtx::new();
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Create 3 atoms
    for i in 0..3 {
        let req = actix_test::TestRequest::post()
            .uri("/api/atoms")
            .insert_header(ctx.auth_header())
            .set_json(json!({ "content": format!("Atom {}", i) }))
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 201);
    }

    // List with limit=2
    let req = actix_test::TestRequest::get()
        .uri("/api/atoms?limit=2")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["total_count"], 3);
    assert_eq!(body["atoms"].as_array().unwrap().len(), 2);
    assert_eq!(body["limit"], 2);
}

#[actix_web::test]
async fn test_update_atom() {
    let ctx = TestCtx::new();
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Create
    let req = actix_test::TestRequest::post()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "content": "original" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let body: Value = actix_test::read_body_json(resp).await;
    let atom_id = body["id"].as_str().unwrap().to_string();

    // Update
    let req = actix_test::TestRequest::put()
        .uri(&format!("/api/atoms/{}", atom_id))
        .insert_header(ctx.auth_header())
        .set_json(json!({ "content": "updated" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["content"], "updated");
}

#[actix_web::test]
async fn test_delete_atom() {
    let ctx = TestCtx::new();
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Create
    let req = actix_test::TestRequest::post()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "content": "to delete" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let body: Value = actix_test::read_body_json(resp).await;
    let atom_id = body["id"].as_str().unwrap().to_string();

    // Delete
    let req = actix_test::TestRequest::delete()
        .uri(&format!("/api/atoms/{}", atom_id))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    // Verify gone
    let req = actix_test::TestRequest::get()
        .uri(&format!("/api/atoms/{}", atom_id))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 404);
}

#[actix_web::test]
async fn test_bulk_create_atoms() {
    let ctx = TestCtx::new();
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/atoms/bulk")
        .insert_header(ctx.auth_header())
        .set_json(json!([
            { "content": "Bulk atom 1" },
            { "content": "Bulk atom 2" },
            { "content": "Bulk atom 3" },
        ]))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);

    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["count"], 3);
    assert_eq!(body["atoms"].as_array().unwrap().len(), 3);
}

#[actix_web::test]
async fn test_create_atom_with_source_url() {
    let ctx = TestCtx::new();
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .set_json(json!({
            "content": "Article content",
            "source_url": "https://example.com/article",
        }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);

    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["source_url"], "https://example.com/article");
}

// ---------------------------------------------------------------------------
// Tag CRUD tests
// ---------------------------------------------------------------------------

#[actix_web::test]
async fn test_create_and_list_tags() {
    let ctx = TestCtx::new();
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Create a tag
    let req = actix_test::TestRequest::post()
        .uri("/api/tags")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "name": "rust" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);

    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["name"], "rust");
    assert!(body["id"].as_str().is_some());

    // List tags (min_count=0 so we see tags with no atoms)
    let req = actix_test::TestRequest::get()
        .uri("/api/tags?min_count=0")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: Value = actix_test::read_body_json(resp).await;
    let tags = body.as_array().unwrap();
    assert!(tags.iter().any(|t| t["name"] == "rust"));
}

#[actix_web::test]
async fn test_create_atom_with_tags() {
    let ctx = TestCtx::new();
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Create a tag first
    let req = actix_test::TestRequest::post()
        .uri("/api/tags")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "name": "testing" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let tag: Value = actix_test::read_body_json(resp).await;
    let tag_id = tag["id"].as_str().unwrap().to_string();

    // Create atom with that tag
    let req = actix_test::TestRequest::post()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .set_json(json!({
            "content": "Tagged content",
            "tag_ids": [tag_id],
        }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);

    let body: Value = actix_test::read_body_json(resp).await;
    let tags = body["tags"].as_array().unwrap();
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0]["name"], "testing");
}

#[actix_web::test]
async fn test_update_tag() {
    let ctx = TestCtx::new();
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Create
    let req = actix_test::TestRequest::post()
        .uri("/api/tags")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "name": "old-name" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let tag: Value = actix_test::read_body_json(resp).await;
    let tag_id = tag["id"].as_str().unwrap().to_string();

    // Update
    let req = actix_test::TestRequest::put()
        .uri(&format!("/api/tags/{}", tag_id))
        .insert_header(ctx.auth_header())
        .set_json(json!({ "name": "new-name" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["name"], "new-name");
}

#[actix_web::test]
async fn test_delete_tag() {
    let ctx = TestCtx::new();
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Create
    let req = actix_test::TestRequest::post()
        .uri("/api/tags")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "name": "ephemeral" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let tag: Value = actix_test::read_body_json(resp).await;
    let tag_id = tag["id"].as_str().unwrap().to_string();

    // Delete
    let req = actix_test::TestRequest::delete()
        .uri(&format!("/api/tags/{}", tag_id))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
}

// ---------------------------------------------------------------------------
// Auth tests
// ---------------------------------------------------------------------------

#[actix_web::test]
async fn test_unauthenticated_request_rejected() {
    let ctx = TestCtx::new();
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::get()
        .uri("/api/atoms")
        .to_request();
    // The middleware may return an actix error (Err) or a proper 401 response (Ok).
    // Handle both: what matters is that the status is 401, not 200.
    match actix_test::try_call_service(&app, req).await {
        Ok(resp) => assert_eq!(resp.status(), 401, "should return 401 Unauthorized"),
        Err(err) => {
            let resp = err.error_response();
            assert_eq!(resp.status(), 401, "should return 401 Unauthorized");
        }
    }
}

// ---------------------------------------------------------------------------
// OpenAPI spec test
// ---------------------------------------------------------------------------

#[actix_web::test]
async fn test_openapi_spec_is_valid() {
    let ctx = TestCtx::new();
    let app = actix_test::init_service(
        App::new()
            .app_data(ctx.state.clone())
            .route(
                "/api/docs/openapi.json",
                web::get().to(atomic_server::openapi_spec),
            ),
    )
    .await;

    let req = actix_test::TestRequest::get()
        .uri("/api/docs/openapi.json")
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["openapi"], "3.1.0");
    assert!(body["paths"]["/api/atoms"].is_object());
    assert!(body["paths"]["/api/atoms/{id}"].is_object());
    assert!(body["paths"]["/api/tags"].is_object());
    assert!(body["components"]["schemas"]["Atom"].is_object());
    assert!(body["components"]["schemas"]["AtomWithTags"].is_object());
}
