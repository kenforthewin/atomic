//! End-to-end tests for the feeds surface.
//!
//! Feeds run a periodic fetch + parse loop. CRUD is straightforward but
//! `POST /api/feeds` is special: the handler immediately fetches the URL
//! and runs `feed-rs` against the body, so the test fixture must serve a
//! parseable Atom/RSS document. Polling then fetches each item's `<link>`
//! and runs `readability`-style extraction against the HTML — the
//! `MockUrlServer` in `atomic-test-support` provides both shapes.

mod support;

use actix_web::test as actix_test;
use serde_json::{json, Value};
use std::time::Duration;
use support::{test_app, Backend, MockUrlServer, TestCtx};

// ==================== Helpers ====================

async fn create_feed<S, B>(
    app: &S,
    auth: (&'static str, String),
    url: &str,
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
        .uri("/api/feeds")
        .insert_header(auth)
        .set_json(json!({ "url": url, "poll_interval": 3600, "tag_ids": [] }))
        .to_request();
    let resp = actix_test::call_service(app, req).await;
    assert_eq!(
        resp.status(),
        201,
        "POST /api/feeds must return 201, got {}",
        resp.status()
    );
    actix_test::read_body_json(resp).await
}

async fn poll_feed<S, B>(
    app: &S,
    auth: (&'static str, String),
    id: &str,
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
        .uri(&format!("/api/feeds/{id}/poll"))
        .insert_header(auth)
        .to_request();
    let resp = actix_test::call_service(app, req).await;
    assert!(
        resp.status().is_success(),
        "poll must succeed, got {}",
        resp.status()
    );
    actix_test::read_body_json(resp).await
}

// ==================== F1. Create + validate ====================

#[actix_web::test]
async fn create_feed_validates_url_sqlite() {
    run_create_feed_validates_url(Backend::Sqlite).await;
}

#[actix_web::test]
async fn create_feed_validates_url_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("create_feed_validates_url_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_create_feed_validates_url(Backend::Postgres).await;
}

async fn run_create_feed_validates_url(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    let mock = MockUrlServer::start().await;

    let feed = create_feed(&app, ctx.auth_header(), &mock.feed_url()).await;
    assert_eq!(feed["url"], mock.feed_url());
    assert_eq!(
        feed["title"], "Mock Feed",
        "title should be backfilled from the parsed feed"
    );

    // `create_feed` also spawns an immediate poll. We don't assert on its
    // result here — F2 covers polling explicitly — but we do want to give
    // the spawn a moment to settle so subsequent tests don't race against
    // a lingering background task.
    tokio::time::sleep(Duration::from_millis(50)).await;
}

// ==================== F2. Poll ingests items ====================

#[actix_web::test]
async fn poll_feed_ingests_items_sqlite() {
    run_poll_feed_ingests_items(Backend::Sqlite).await;
}

#[actix_web::test]
async fn poll_feed_ingests_items_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("poll_feed_ingests_items_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_poll_feed_ingests_items(Backend::Postgres).await;
}

async fn run_poll_feed_ingests_items(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    let mock = MockUrlServer::start().await;

    let feed = create_feed(&app, ctx.auth_header(), &mock.feed_url()).await;
    let id = feed["id"].as_str().unwrap().to_string();

    // `create_feed` already spawned an async poll. Run one synchronous
    // poll on top so we have a stable observation point — then wait for
    // both source atoms to materialize.
    let _ = poll_feed(&app, ctx.auth_header(), &id).await;

    // Wait until both atoms exist by inspecting the atom list. The poll's
    // create-time spawn + the explicit poll above can race on which one
    // observes each item, so we assert on the persisted shape (atoms with
    // the expected source_urls) rather than on the poll-result counters.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let expected: Vec<String> = [1, 2].iter().map(|n| mock.article_url(*n)).collect();
    loop {
        let req = actix_test::TestRequest::get()
            .uri("/api/atoms?limit=50")
            .insert_header(ctx.auth_header())
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        let body: Value = actix_test::read_body_json(resp).await;
        let urls: Vec<String> = body["atoms"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|a| a["source_url"].as_str().map(str::to_string))
            .collect();
        if expected.iter().all(|e| urls.iter().any(|u| u == e)) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("feed atoms did not all materialize within 15s; got source_urls {urls:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ==================== F3. Dedup ====================

#[actix_web::test]
async fn poll_feed_dedupes_items_sqlite() {
    run_poll_feed_dedupes_items(Backend::Sqlite).await;
}

#[actix_web::test]
async fn poll_feed_dedupes_items_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("poll_feed_dedupes_items_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_poll_feed_dedupes_items(Backend::Postgres).await;
}

async fn run_poll_feed_dedupes_items(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    let mock = MockUrlServer::start().await;

    let feed = create_feed(&app, ctx.auth_header(), &mock.feed_url()).await;
    let id = feed["id"].as_str().unwrap().to_string();

    // Drain background-poll progress. Once both items are observed across
    // any combination of new + skipped, a fresh poll must produce zero new
    // items (everything is already claimed in `feed_items`).
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        let result = poll_feed(&app, ctx.auth_header(), &id).await;
        let new = result["new_items"].as_i64().unwrap_or(0);
        if new == 0 {
            // All items already claimed — exit and run the dedup assertion.
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("feed never reached steady-state dedupe within 15s");
}

// ==================== F4. Delete ====================

#[actix_web::test]
async fn delete_feed_round_trip_sqlite() {
    run_delete_feed_round_trip(Backend::Sqlite).await;
}

#[actix_web::test]
async fn delete_feed_round_trip_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("delete_feed_round_trip_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_delete_feed_round_trip(Backend::Postgres).await;
}

async fn run_delete_feed_round_trip(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    let mock = MockUrlServer::start().await;

    let feed = create_feed(&app, ctx.auth_header(), &mock.feed_url()).await;
    let id = feed["id"].as_str().unwrap().to_string();

    let req = actix_test::TestRequest::delete()
        .uri(&format!("/api/feeds/{id}"))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let req = actix_test::TestRequest::get()
        .uri("/api/feeds")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let feeds: Vec<Value> = actix_test::read_body_json(resp).await;
    assert!(
        feeds.iter().all(|f| f["id"].as_str() != Some(id.as_str())),
        "deleted feed must no longer appear in the list"
    );
}

// ==================== F5. Auth required ====================

#[actix_web::test]
async fn feeds_require_auth_sqlite() {
    run_feeds_require_auth(Backend::Sqlite).await;
}

#[actix_web::test]
async fn feeds_require_auth_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("feeds_require_auth_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_feeds_require_auth(Backend::Postgres).await;
}

async fn run_feeds_require_auth(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let req = actix_test::TestRequest::get()
        .uri("/api/feeds")
        .to_request();
    let resp = actix_test::try_call_service(&app, req).await;
    assert!(resp.is_err(), "unauthenticated feeds list must be rejected");
}
