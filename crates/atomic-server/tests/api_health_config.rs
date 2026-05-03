//! Integration tests for the health-config REST endpoints.
//!
//! GET /api/health/config → HealthConfig (defaults if unset)
//! PUT /api/health/config → round-trip persistence + threshold validation (400 on bad input)

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
async fn test_health_config_defaults_on_first_read() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::get()
        .uri("/api/health/config")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: Value = actix_test::read_body_json(resp).await;
    // All threshold defaults must be present for forward-compat.
    let t = &body["thresholds"];
    assert_eq!(t["boilerplate_similarity"].as_f64().unwrap(), 0.99);
    assert_eq!(t["boilerplate_min_clones"].as_i64().unwrap(), 2);
    assert_eq!(t["content_quality_short_chars"].as_i64().unwrap(), 100);
    assert_eq!(t["content_quality_long_chars"].as_i64().unwrap(), 15_000);
    assert_eq!(t["wiki_min_atoms_per_tag"].as_i64().unwrap(), 5);
    assert_eq!(t["semantic_graph_freshness_warning"].as_i64().unwrap(), 20);
}

#[actix_web::test]
async fn test_health_config_put_round_trip() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Start from defaults and override a subset.
    let payload = json!({
        "overrides": {},
        "thresholds": {
            "boilerplate_similarity": 0.995,
            "boilerplate_min_clones": 3,
            "contradiction_similarity_min": 0.80,
            "contradiction_similarity_max": 0.92,
            "contradiction_shared_tags_min": 1,
            "content_overlap_similarity_min": 0.55,
            "content_overlap_similarity_max": 0.85,
            "content_overlap_shared_tags_min": 2,
            "content_quality_short_chars": 150,
            "content_quality_long_chars": 20_000,
            "wiki_min_atoms_per_tag": 10,
            "tag_health_single_atom_threshold": 5,
            "semantic_graph_freshness_warning": 50
        }
    });

    let req = actix_test::TestRequest::put()
        .uri("/api/health/config")
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
        .uri("/api/health/config")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: Value = actix_test::read_body_json(resp).await;
    let t = &body["thresholds"];
    assert_eq!(t["boilerplate_similarity"].as_f64().unwrap(), 0.995);
    assert_eq!(t["boilerplate_min_clones"].as_i64().unwrap(), 3);
    assert_eq!(t["content_quality_short_chars"].as_i64().unwrap(), 150);
    assert_eq!(t["wiki_min_atoms_per_tag"].as_i64().unwrap(), 10);
    assert_eq!(t["semantic_graph_freshness_warning"].as_i64().unwrap(), 50);
}

#[actix_web::test]
async fn test_health_config_rejects_similarity_out_of_range() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let bad = json!({
        "overrides": {},
        "thresholds": {
            "boilerplate_similarity": 1.5,
            "boilerplate_min_clones": 2,
            "contradiction_similarity_min": 0.80,
            "contradiction_similarity_max": 0.92,
            "contradiction_shared_tags_min": 1,
            "content_overlap_similarity_min": 0.55,
            "content_overlap_similarity_max": 0.85,
            "content_overlap_shared_tags_min": 2,
            "content_quality_short_chars": 100,
            "content_quality_long_chars": 15_000,
            "wiki_min_atoms_per_tag": 5,
            "tag_health_single_atom_threshold": 3,
            "semantic_graph_freshness_warning": 20
        }
    });

    let req = actix_test::TestRequest::put()
        .uri("/api/health/config")
        .insert_header(ctx.auth_header())
        .set_json(&bad)
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);

    let body: Value = actix_test::read_body_json(resp).await;
    assert!(
        body["error"].as_str().unwrap_or("").contains("boilerplate_similarity"),
        "expected boilerplate_similarity in error, got {}",
        body
    );
}

#[actix_web::test]
async fn test_health_config_rejects_inverted_window() {
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let bad = json!({
        "overrides": {},
        "thresholds": {
            "boilerplate_similarity": 0.99,
            "boilerplate_min_clones": 2,
            "contradiction_similarity_min": 0.95,
            "contradiction_similarity_max": 0.90,
            "contradiction_shared_tags_min": 1,
            "content_overlap_similarity_min": 0.55,
            "content_overlap_similarity_max": 0.85,
            "content_overlap_shared_tags_min": 2,
            "content_quality_short_chars": 100,
            "content_quality_long_chars": 15_000,
            "wiki_min_atoms_per_tag": 5,
            "tag_health_single_atom_threshold": 3,
            "semantic_graph_freshness_warning": 20
        }
    });

    let req = actix_test::TestRequest::put()
        .uri("/api/health/config")
        .insert_header(ctx.auth_header())
        .set_json(&bad)
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 400);
}

#[actix_web::test]
async fn test_health_config_partial_payload_uses_defaults() {
    // Demonstrates serde-default forward-compat: client sends only the fields
    // it knows about, server fills the rest from defaults.
    let ctx = TestCtx::new().await;
    let app = actix_test::init_service(test_app(&ctx)).await;

    let payload = json!({
        "overrides": {},
        "thresholds": {
            "boilerplate_similarity": 0.995
        }
    });

    let req = actix_test::TestRequest::put()
        .uri("/api/health/config")
        .insert_header(ctx.auth_header())
        .set_json(&payload)
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "PUT failed: {}", resp.status());

    let req = actix_test::TestRequest::get()
        .uri("/api/health/config")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let body: Value = actix_test::read_body_json(resp).await;
    let t = &body["thresholds"];
    assert_eq!(t["boilerplate_similarity"].as_f64().unwrap(), 0.995);
    // Unset fields fell back to defaults.
    assert_eq!(t["boilerplate_min_clones"].as_i64().unwrap(), 2);
    assert_eq!(t["wiki_min_atoms_per_tag"].as_i64().unwrap(), 5);
}
