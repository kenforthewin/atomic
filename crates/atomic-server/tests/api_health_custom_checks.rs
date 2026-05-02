//! Integration tests for the custom-health-checks REST endpoints.
//!
//! GET /api/health/custom-checks → Vec<CustomCheck>
//! PUT /api/health/custom-checks → round-trip persistence

use actix_web::{test as actix_test, web, App};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::broadcast;

struct TestCtx {
    _temp: tempfile::TempDir,
    state: web::Data<atomic_server::state::AppState>,
    token: String,
}

impl TestCtx {
    async fn new() -> Self {
        let temp = tempfile::TempDir::new().unwrap();
        let manager = Arc::new(atomic_core::DatabaseManager::new(temp.path()).unwrap());
        let (_info, raw_token) = manager
            .active_core()
            .await
            .unwrap()
            .create_api_token("test")
            .await
            .unwrap();
        let (event_tx, _) = broadcast::channel(16);
        let state = web::Data::new(atomic_server::state::AppState {
            manager,
            event_tx,
            public_url: None,
            log_buffer: atomic_server::log_buffer::LogBuffer::new(16),
            export_jobs: atomic_server::export_jobs::ExportJobManager::for_tests(
                temp.path().join("exports"),
            ),
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
    App::new().app_data(ctx.state.clone()).service(
        web::scope("/api")
            .wrap(atomic_server::auth::BearerAuth {
                state: ctx.state.clone(),
            })
            .configure(atomic_server::routes::configure_routes),
    )
}

#[actix_web::test]
async fn test_custom_checks_get_default_empty() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::get()
        .uri("/api/health/custom-checks")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["checks"].as_array().unwrap().len(), 0);
}

#[actix_web::test]
async fn test_custom_checks_put_and_get_round_trip() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let payload = json!({
        "checks": [{
            "id": "needs_source",
            "label": "Requires source URL",
            "description": "All atoms must carry a source_url.",
            "enabled": true,
            "weight": 0.5,
            "rule": { "kind": "require_source", "tag_filter": null }
        }, {
            "id": "tagged_enough",
            "label": "Tag count bounds",
            "description": "",
            "enabled": false,
            "weight": 0.0,
            "rule": { "kind": "tag_cardinality", "min": 1, "max": 5, "tag_filter": null }
        }]
    });

    let req = actix_test::TestRequest::put()
        .uri("/api/health/custom-checks")
        .insert_header(ctx.auth_header())
        .set_json(&payload)
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "PUT failed: {}",
        resp.status()
    );

    let req = actix_test::TestRequest::get()
        .uri("/api/health/custom-checks")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: Value = actix_test::read_body_json(resp).await;
    let arr = body["checks"].as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["id"], "needs_source");
    assert_eq!(arr[0]["rule"]["kind"], "require_source");
    assert_eq!(arr[1]["id"], "tagged_enough");
    assert_eq!(arr[1]["rule"]["min"], 1);
    assert_eq!(arr[1]["rule"]["max"], 5);
    assert_eq!(arr[1]["enabled"], false);
}

#[actix_web::test]
async fn test_custom_checks_put_overwrites_previous() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let first = json!({
        "checks": [{
            "id": "a",
            "label": "A",
            "description": "",
            "enabled": true,
            "weight": 1.0,
            "rule": { "kind": "require_source", "tag_filter": null }
        }]
    });
    let req = actix_test::TestRequest::put()
        .uri("/api/health/custom-checks")
        .insert_header(ctx.auth_header())
        .set_json(&first)
        .to_request();
    assert!(actix_test::call_service(&app, req).await.status().is_success());

    let second = json!({ "checks": [] });
    let req = actix_test::TestRequest::put()
        .uri("/api/health/custom-checks")
        .insert_header(ctx.auth_header())
        .set_json(&second)
        .to_request();
    assert!(actix_test::call_service(&app, req).await.status().is_success());

    let req = actix_test::TestRequest::get()
        .uri("/api/health/custom-checks")
        .insert_header(ctx.auth_header())
        .to_request();
    let body: Value = actix_test::read_body_json(actix_test::call_service(&app, req).await).await;
    assert_eq!(body["checks"].as_array().unwrap().len(), 0);
}

#[actix_web::test]
async fn test_custom_checks_requires_auth() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::get()
        .uri("/api/health/custom-checks")
        .to_request();
    match actix_test::try_call_service(&app, req).await {
        Ok(resp) => assert_eq!(resp.status(), 401),
        Err(err) => {
            let resp = err.error_response();
            assert_eq!(resp.status(), 401);
        }
    }
}


// --- Preview ---------------------------------------------------------------

#[actix_web::test]
async fn test_custom_checks_preview_returns_counts() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Seed two atoms — one missing source_url so RequireSource flags it.
    let req = actix_test::TestRequest::post()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "content": "no source" }))
        .to_request();
    assert!(actix_test::call_service(&app, req).await.status().is_success());
    let req = actix_test::TestRequest::post()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "content": "has source", "source_url": "https://example.com/a" }))
        .to_request();
    assert!(actix_test::call_service(&app, req).await.status().is_success());

    let req = actix_test::TestRequest::post()
        .uri("/api/health/custom-checks/preview")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "rule": { "kind": "require_source", "tag_filter": null } }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["total_considered"], 2);
    assert_eq!(body["flagged_count"], 1);
    assert_eq!(body["sample"].as_array().unwrap().len(), 1);

    // Preview must not persist — list should still be empty.
    let req = actix_test::TestRequest::get()
        .uri("/api/health/custom-checks")
        .insert_header(ctx.auth_header())
        .to_request();
    let body: Value = actix_test::read_body_json(actix_test::call_service(&app, req).await).await;
    assert_eq!(body["checks"].as_array().unwrap().len(), 0);
}

#[actix_web::test]
async fn test_custom_checks_preview_returns_error_for_malformed_regex() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::post()
        .uri("/api/health/custom-checks/preview")
        .insert_header(ctx.auth_header())
        .set_json(json!({
            "rule": { "kind": "content_regex", "pattern": "(?P<unterminated", "invert": false }
        }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(!resp.status().is_success(), "malformed regex should error");
}