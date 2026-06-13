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
use atomic_core::ingest::poller::FEED_POLL_TASK_ID;
use atomic_core::{AtomicCore, TaskRun, TaskRunState, TaskRunTrigger};
use serde_json::{json, Value};
use std::time::Duration;
use support::{test_app, Backend, MockUrlServer, TestCtx};

// ==================== Helpers ====================

async fn create_feed_with_interval<S, B>(
    app: &S,
    auth: (&'static str, String),
    url: &str,
    poll_interval: i32,
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
        .set_json(json!({ "url": url, "poll_interval": poll_interval, "tag_ids": [] }))
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

async fn create_feed<S, B>(app: &S, auth: (&'static str, String), url: &str) -> Value
where
    S: actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse<B>,
        Error = actix_web::Error,
    >,
    B: actix_web::body::MessageBody,
{
    create_feed_with_interval(app, auth, url, 3600).await
}

/// POST /api/feeds/{id}/poll. Returns `Some(result)` on 200 and `None` on
/// 409 — manual polls ride the `task_runs` ledger, so a poll already in
/// flight (e.g. the create-time kickoff) or a failed poll inside its
/// backoff window is reported as a conflict rather than double-polled.
async fn poll_feed<S, B>(app: &S, auth: (&'static str, String), id: &str) -> Option<Value>
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
    match resp.status().as_u16() {
        200 => Some(actix_test::read_body_json(resp).await),
        409 => None,
        other => panic!("poll must return 200 or 409, got {other}"),
    }
}

async fn active_core(ctx: &TestCtx) -> AtomicCore {
    ctx.state.manager.active_core().await.expect("active core")
}

/// Poll the feed's `feed_poll` ledger history until `pred` accepts it.
/// Used to wait out the create-time kickoff poll, which runs on a spawned
/// task and would otherwise race the test's own claims.
async fn wait_for_runs(
    core: &AtomicCore,
    feed_id: &str,
    what: &str,
    pred: impl Fn(&[TaskRun]) -> bool,
) -> Vec<TaskRun> {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let runs = core
            .list_task_runs(FEED_POLL_TASK_ID, Some(feed_id), 20)
            .await
            .expect("list_task_runs");
        if pred(&runs) {
            return runs;
        }
        if std::time::Instant::now() >= deadline {
            panic!("{what} not reached within 15s; runs: {runs:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ==================== F1. Create + validate ====================

#[actix_web::test]
async fn create_feed_validates_url_sqlite() {
    run_create_feed_validates_url(Backend::Sqlite).await;
}

#[actix_web::test]
async fn create_feed_validates_url_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "create_feed_validates_url_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
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
    // items (everything is already claimed in `feed_items`). A `None` poll
    // result means the create-time kickoff still holds the ledger lease —
    // keep waiting.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        if let Some(result) = poll_feed(&app, ctx.auth_header(), &id).await {
            let new = result["new_items"].as_i64().unwrap_or(0);
            if new == 0 {
                // All items already claimed — exit and run the dedup assertion.
                return;
            }
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

// ==================== F6. Polls record task_runs rows per feed ====================

#[actix_web::test]
async fn poll_records_feed_run_rows_sqlite() {
    run_poll_records_feed_run_rows(Backend::Sqlite).await;
}

#[actix_web::test]
async fn poll_records_feed_run_rows_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "poll_records_feed_run_rows_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_poll_records_feed_run_rows(Backend::Postgres).await;
}

async fn run_poll_records_feed_run_rows(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    let mock = MockUrlServer::start().await;

    let feed = create_feed(&app, ctx.auth_header(), &mock.feed_url()).await;
    let id = feed["id"].as_str().unwrap().to_string();
    let core = active_core(&ctx).await;

    // The create-time kickoff poll already rides the ledger. Wait for it to
    // settle so the explicit poll below deterministically claims a fresh
    // row instead of racing the kickoff's lease.
    wait_for_runs(&core, &id, "kickoff poll success", |runs| {
        runs.iter().any(|r| r.state == TaskRunState::Succeeded)
    })
    .await;

    let result = poll_feed(&app, ctx.auth_header(), &id)
        .await
        .expect("no competing poll once the kickoff has settled");
    assert_eq!(result["feed_id"].as_str(), Some(id.as_str()));

    let history = core
        .list_task_runs(FEED_POLL_TASK_ID, Some(&id), 20)
        .await
        .unwrap();
    assert_eq!(history.len(), 2, "kickoff poll + explicit poll");
    for run in &history {
        assert_eq!(run.subject_id.as_deref(), Some(id.as_str()));
        assert_eq!(run.state, TaskRunState::Succeeded);
        assert_eq!(run.trigger, TaskRunTrigger::Manual);
        assert!(run.finished_at.is_some());
        assert!(run.lease_until.is_none(), "terminal rows clear the lease");
    }

    // The definition row's fast-path cache reflects the settled polls.
    let req = actix_test::TestRequest::get()
        .uri(&format!("/api/feeds/{id}"))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    let body: Value = actix_test::read_body_json(resp).await;
    assert!(
        body["last_polled_at"].is_string(),
        "success advances last_polled_at"
    );
    assert!(body["last_error"].is_null());
}

// ==================== F7. Failing feed backs off; healthy feed unaffected ====================

#[actix_web::test]
async fn failing_feed_backs_off_while_healthy_feed_polls_sqlite() {
    run_failing_feed_backs_off_while_healthy_feed_polls(Backend::Sqlite).await;
}

#[actix_web::test]
async fn failing_feed_backs_off_while_healthy_feed_polls_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "failing_feed_backs_off_while_healthy_feed_polls_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_failing_feed_backs_off_while_healthy_feed_polls(Backend::Postgres).await;
}

async fn run_failing_feed_backs_off_while_healthy_feed_polls(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    let mock = MockUrlServer::start().await;

    // The flaky feed parses at create time (validate-on-create) and 500s
    // on every poll after; interval 0 makes both feeds due on every sweep.
    let bad = create_feed_with_interval(&app, ctx.auth_header(), &mock.flaky_feed_url(), 0).await;
    let good = create_feed_with_interval(&app, ctx.auth_header(), &mock.feed_url(), 0).await;
    let bad_id = bad["id"].as_str().unwrap().to_string();
    let good_id = good["id"].as_str().unwrap().to_string();
    let core = active_core(&ctx).await;

    // Wait for the create-time kickoff polls to settle: the bad feed's
    // fails and goes pending-with-backoff, the good feed's succeeds.
    let bad_runs = wait_for_runs(&core, &bad_id, "bad kickoff failure", |runs| {
        runs.iter()
            .any(|r| r.state == TaskRunState::Pending && r.attempts == 1)
    })
    .await;
    wait_for_runs(&core, &good_id, "good kickoff success", |runs| {
        runs.iter().any(|r| r.state == TaskRunState::Succeeded)
    })
    .await;

    let bad_run = &bad_runs[0];
    assert_eq!(bad_run.subject_id.as_deref(), Some(bad_id.as_str()));
    assert!(bad_run.last_error.is_some());
    assert!(
        bad_run.next_attempt_at.as_str() > chrono::Utc::now().to_rfc3339().as_str(),
        "failure pushed next_attempt_at into the future (backoff)"
    );

    // Fast-path cache: a retryable failure stamps last_error but must not
    // advance last_polled_at — the feed stays due and the ledger throttles.
    let bad_feed = core.get_feed(&bad_id).await.unwrap();
    assert!(bad_feed.last_polled_at.is_none());
    assert!(bad_feed.last_error.is_some());

    let good_before = core
        .list_task_runs(FEED_POLL_TASK_ID, Some(&good_id), 20)
        .await
        .unwrap()
        .len();

    // Drive one sweep directly — the tick body main.rs runs every 60s. The
    // bad feed is due but inside its backoff window, so it's skipped; the
    // good feed polls again.
    let polled = core.poll_due_feeds(|_| {}, |_| {}).await;
    assert!(
        polled.iter().any(|r| r.feed_id == good_id),
        "healthy feed polled by the sweep"
    );
    assert!(
        polled.iter().all(|r| r.feed_id != bad_id),
        "backed-off feed must not poll"
    );

    let bad_history = core
        .list_task_runs(FEED_POLL_TASK_ID, Some(&bad_id), 20)
        .await
        .unwrap();
    assert_eq!(
        bad_history.len(),
        1,
        "backed-off row reused, not duplicated"
    );
    assert_eq!(bad_history[0].attempts, 1, "no re-attempt inside backoff");

    let good_history = core
        .list_task_runs(FEED_POLL_TASK_ID, Some(&good_id), 20)
        .await
        .unwrap();
    assert_eq!(
        good_history.len(),
        good_before + 1,
        "healthy feed recorded a sweep run"
    );
    assert!(good_history
        .iter()
        .all(|r| r.state == TaskRunState::Succeeded));
    // Newest first: the sweep run carries the schedule trigger, unlike the
    // manual kickoff.
    assert_eq!(good_history[0].trigger, TaskRunTrigger::Schedule);
}

// ==================== F8. Concurrent sweeps dedup on the live lease ====================

#[actix_web::test]
async fn concurrent_sweeps_poll_each_due_feed_once_sqlite() {
    run_concurrent_sweeps_poll_each_due_feed_once(Backend::Sqlite).await;
}

#[actix_web::test]
async fn concurrent_sweeps_poll_each_due_feed_once_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "concurrent_sweeps_poll_each_due_feed_once_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_concurrent_sweeps_poll_each_due_feed_once(Backend::Postgres).await;
}

async fn run_concurrent_sweeps_poll_each_due_feed_once(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;
    let mock = MockUrlServer::start().await;

    // Slow fixture: the winning sweep's poll stays in flight long enough
    // that the losing sweep's claim deterministically sees a live lease.
    let feed = create_feed_with_interval(&app, ctx.auth_header(), &mock.slow_feed_url(), 0).await;
    let id = feed["id"].as_str().unwrap().to_string();
    let core = active_core(&ctx).await;

    wait_for_runs(&core, &id, "kickoff poll success", |runs| {
        runs.iter().any(|r| r.state == TaskRunState::Succeeded)
    })
    .await;

    // Two overlapping sweeps — e.g. two server processes sharing one
    // database. Exactly one may poll the due feed.
    let (a, b) = tokio::join!(
        core.poll_due_feeds(|_| {}, |_| {}),
        core.poll_due_feeds(|_| {}, |_| {})
    );
    assert_eq!(a.len() + b.len(), 1, "exactly one sweep polled the feed");

    let history = core
        .list_task_runs(FEED_POLL_TASK_ID, Some(&id), 20)
        .await
        .unwrap();
    assert_eq!(
        history.len(),
        2,
        "kickoff + one sweep run — the overlapping sweep deduped on the live lease"
    );
    assert!(history.iter().all(|r| r.state == TaskRunState::Succeeded));
}
