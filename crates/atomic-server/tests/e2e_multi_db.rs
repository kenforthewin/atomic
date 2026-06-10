//! Multi-database isolation through the HTTP layer.
//!
//! `AppState::resolve_core` picks the `AtomicCore` for a request based on the
//! `X-Atomic-Database` header. The lower-level `Db` extractor and storage
//! `db_id` filter are already covered by `atomic-core/tests/multi_db_tests.rs`
//! — this suite proves the routing is honored through the full server stack
//! (auth wrapper, route handler, request extension flow).

mod support;

use actix_web::test as actix_test;
use serde_json::{json, Value};
use std::collections::HashSet;
use support::{test_app, Backend, TestCtx};

#[actix_web::test]
async fn x_atomic_database_header_isolates_atoms_sqlite() {
    run_x_atomic_database_header_isolates_atoms(Backend::Sqlite).await;
}

#[actix_web::test]
async fn x_atomic_database_header_isolates_atoms_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "x_atomic_database_header_isolates_atoms_postgres: skipping \
             (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_x_atomic_database_header_isolates_atoms(Backend::Postgres).await;
}

async fn run_x_atomic_database_header_isolates_atoms(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Create two named databases via the REST endpoint — going through the
    // HTTP layer here too so the AppState's manager handle is exercised end
    // to end.
    let alpha_id = create_database(&app, &ctx, "alpha").await;
    let beta_id = create_database(&app, &ctx, "beta").await;
    assert_ne!(alpha_id, beta_id, "new databases must get distinct ids");

    // Post one atom to each database via the routing header.
    let alpha_atom = post_atom(&app, &ctx, &alpha_id, "alpha-only content").await;
    let beta_atom = post_atom(&app, &ctx, &beta_id, "beta-only content").await;

    // Listing alpha must show only alpha's atom (and vice versa). Compare on
    // ids — names/other fields may collide.
    let alpha_ids = list_atom_ids(&app, &ctx, &alpha_id).await;
    let beta_ids = list_atom_ids(&app, &ctx, &beta_id).await;

    assert!(
        alpha_ids.contains(&alpha_atom),
        "alpha listing must include alpha's atom; got {alpha_ids:?}"
    );
    assert!(
        !alpha_ids.contains(&beta_atom),
        "alpha listing MUST NOT include beta's atom (cross-DB leak); got {alpha_ids:?}"
    );
    assert!(
        beta_ids.contains(&beta_atom),
        "beta listing must include beta's atom; got {beta_ids:?}"
    );
    assert!(
        !beta_ids.contains(&alpha_atom),
        "beta listing MUST NOT include alpha's atom (cross-DB leak); got {beta_ids:?}"
    );

    // Cross-DB GET by id must also miss — the route handler resolves the core
    // via the header, then queries that core's storage with the atom id.
    let req = actix_test::TestRequest::get()
        .uri(&format!("/api/atoms/{}", alpha_atom))
        .insert_header(ctx.auth_header())
        .insert_header(ctx.db_header(&beta_id))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        404,
        "alpha's atom must 404 when requested with beta's db header"
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn create_database<S, B>(app: &S, ctx: &TestCtx, name: &str) -> String
where
    S: actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse<B>,
        Error = actix_web::Error,
    >,
    B: actix_web::body::MessageBody,
{
    let req = actix_test::TestRequest::post()
        .uri("/api/databases")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "name": name }))
        .to_request();
    let resp = actix_test::call_service(app, req).await;
    assert_eq!(
        resp.status(),
        201,
        "POST /api/databases should return 201 for {name}"
    );
    let body: Value = actix_test::read_body_json(resp).await;
    body["id"]
        .as_str()
        .expect("created database has id")
        .to_string()
}

async fn post_atom<S, B>(app: &S, ctx: &TestCtx, db_id: &str, content: &str) -> String
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
        .insert_header(ctx.auth_header())
        .insert_header(ctx.db_header(db_id))
        .set_json(json!({ "content": content }))
        .to_request();
    let resp = actix_test::call_service(app, req).await;
    assert_eq!(
        resp.status(),
        201,
        "POST /api/atoms to db {db_id} should return 201"
    );
    let body: Value = actix_test::read_body_json(resp).await;
    body["id"].as_str().expect("created atom has id").to_string()
}

async fn list_atom_ids<S, B>(app: &S, ctx: &TestCtx, db_id: &str) -> HashSet<String>
where
    S: actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse<B>,
        Error = actix_web::Error,
    >,
    B: actix_web::body::MessageBody,
{
    let req = actix_test::TestRequest::get()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .insert_header(ctx.db_header(db_id))
        .to_request();
    let resp = actix_test::call_service(app, req).await;
    assert_eq!(resp.status(), 200, "GET /api/atoms should succeed");
    let body: Value = actix_test::read_body_json(resp).await;
    body["atoms"]
        .as_array()
        .expect("listing has atoms array")
        .iter()
        .filter_map(|a| a["id"].as_str().map(str::to_string))
        .collect()
}
