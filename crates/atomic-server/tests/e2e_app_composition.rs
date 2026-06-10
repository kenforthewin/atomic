//! End-to-end coverage for the composed application route table.
//!
//! `support::test_app` builds its `App` from
//! `atomic_server::app::configure_app` — the same function `main.rs` uses —
//! so this suite pins the full composition in one place: a public route
//! (`/health`) answers without auth, the bearer-gated `/api` scope accepts
//! a valid token and rejects a missing one, and the `/mcp` scope sits
//! behind `McpAuth` (401 with `WWW-Authenticate` when unauthenticated).
//! Every other e2e suite exercises the same wiring implicitly; this one
//! asserts the cross-scope layout directly so a regression in the
//! composition itself (e.g. a route slipping inside the wrong auth scope)
//! fails loudly rather than as a confusing downstream test error.
//!
//! It also pins the contracts a caller composing these routes relies on:
//! the `RequestDatabaseManager` / `RequestEventChannel` request-extension
//! overrides (for `Db` resolution, the manager-plane routes, handlers'
//! event publishing, and the WS subscription), and the granular pieces
//! (`configure_public_routes`, `mcp_scope`, `api_scope`) composing to the
//! same behavior as the all-in-one `configure_app`.

mod support;

use std::sync::Arc;
use std::time::Duration;

use actix_web::dev::ServiceRequest;
use actix_web::middleware::{from_fn, Next};
use actix_web::test as actix_test;
use actix_web::{App, HttpMessage};
use atomic_core::DatabaseManager;
use atomic_server::app::{api_scope, configure_app, configure_public_routes, mcp_scope};
use atomic_server::auth::BearerAuth;
use atomic_server::db_extractor::RequestDatabaseManager;
use atomic_server::event_channel::RequestEventChannel;
use atomic_server::mcp_auth::McpAuth;
use atomic_server::state::ServerEvent;
use futures_util::SinkExt;
use serde_json::{json, Value};
use support::{
    collect_ws_event_until, mcp_transport_for, spawn_live_server_with_event_channel, test_app,
    Backend, TestCtx,
};
use tokio::sync::broadcast;

#[actix_web::test]
async fn full_composition_sqlite() {
    run_full_composition(Backend::Sqlite).await;
}

#[actix_web::test]
async fn full_composition_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("full_composition_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_full_composition(Backend::Postgres).await;
}

async fn run_full_composition(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Public route: /health answers without any credentials.
    let req = actix_test::TestRequest::get().uri("/health").to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "/health must be public");
    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));

    // Authenticated route: the /api scope admits a valid bearer token...
    let req = actix_test::TestRequest::get()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "/api must work with a valid token");

    // ...and rejects the same request without one. BearerAuth surfaces the
    // rejection as an actix error (rendered as a 401 over HTTP), so probe
    // the error's status rather than a ServiceResponse.
    let req = actix_test::TestRequest::get()
        .uri("/api/atoms")
        .to_request();
    let err = match actix_test::try_call_service(&app, req).await {
        Ok(resp) => panic!("/api must reject missing tokens, got {}", resp.status()),
        Err(err) => err,
    };
    assert_eq!(
        err.as_response_error().error_response().status(),
        401,
        "/api rejection must render as 401"
    );

    // MCP scope: McpAuth gates /mcp and advertises OAuth discovery on 401.
    let req = actix_test::TestRequest::post().uri("/mcp").to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401, "/mcp must reject missing tokens");
    let www_authenticate = resp
        .headers()
        .get("WWW-Authenticate")
        .and_then(|v| v.to_str().ok())
        .expect("MCP 401 must carry WWW-Authenticate");
    assert!(
        www_authenticate.contains("resource_metadata="),
        "MCP 401 must point at OAuth discovery, got: {www_authenticate}"
    );
}

// ==================== Injected database-manager resolution ====================

#[actix_web::test]
async fn injected_manager_resolution_sqlite() {
    run_injected_manager_resolution(Backend::Sqlite).await;
}

#[actix_web::test]
async fn injected_manager_resolution_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "injected_manager_resolution_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_injected_manager_resolution(Backend::Postgres).await;
}

/// `Db` resolution contract for embedders: a layer composing
/// `configure_app` under its own middleware can install a
/// [`RequestDatabaseManager`] per request, and the `Db` extractor resolves
/// against that manager instead of `AppState`'s; with no such middleware
/// (the standalone server), it falls back to `AppState`.
///
/// Two managers over the *same* storage with different active databases
/// make the override observable through the standard routes. (Two fully
/// disjoint `TestCtx`s would be starker, but the Postgres backend shares
/// one physical database per process — constructing a second context
/// truncates the first — so the active-database split is the portable way
/// to tell the managers apart.)
async fn run_injected_manager_resolution(backend: Backend) {
    let is_postgres = matches!(backend, Backend::Postgres);
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Seed one atom in the default database and one in a second database.
    let alpha_id = post_atom(&app, &ctx, None, "alpha — lives in the default database").await;
    let probe_db = create_database(&app, &ctx, "resolution-probe").await;
    let beta_id = post_atom(
        &app,
        &ctx,
        Some(&probe_db),
        "beta — lives in the probe database",
    )
    .await;

    // No middleware installed → the extractor falls back to AppState, whose
    // active database is the default. The rest of the suite relies on this
    // implicitly; assert it once, explicitly, next to the override test.
    let ids = list_atom_ids(&app, &ctx, None).await;
    assert!(
        ids.contains(&alpha_id) && !ids.contains(&beta_id),
        "without middleware, Db must resolve via AppState's active database"
    );

    // A second manager over the same storage, with the probe database active.
    let manager = if is_postgres {
        let url = std::env::var("ATOMIC_TEST_DATABASE_URL").expect("postgres url");
        Arc::new(
            DatabaseManager::new_postgres(ctx.data_dir(), &url)
                .await
                .expect("open second postgres manager"),
        )
    } else {
        Arc::new(DatabaseManager::new(ctx.data_dir()).expect("open second sqlite manager"))
    };
    manager
        .set_active(&probe_db)
        .await
        .expect("activate probe db");

    // The same route table, wrapped in a middleware that installs the second
    // manager — the composition shape an embedder would use.
    let app = init_app_with_injected_manager(&ctx, Arc::clone(&manager)).await;

    // The extractor now resolves against the injected manager, whose active
    // database is the probe — not AppState's default.
    let ids = list_atom_ids(&app, &ctx, None).await;
    assert!(
        ids.contains(&beta_id) && !ids.contains(&alpha_id),
        "with middleware, Db must resolve via the injected manager's active database"
    );

    // The per-request selection rules (X-Atomic-Database header) apply to
    // the injected manager too — the override swaps the manager, not the
    // resolution logic.
    let default_db = ctx.state.manager.active_id().expect("default db id");
    let ids = list_atom_ids(&app, &ctx, Some(&default_db)).await;
    assert!(
        ids.contains(&alpha_id) && !ids.contains(&beta_id),
        "X-Atomic-Database must select within the injected manager"
    );
}

// ============= Injected manager: manager-plane routes =============

#[actix_web::test]
async fn databases_routes_honor_injected_manager_sqlite() {
    run_databases_routes_honor_injected_manager(Backend::Sqlite).await;
}

#[actix_web::test]
async fn databases_routes_honor_injected_manager_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "databases_routes_honor_injected_manager_postgres: skipping \
             (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_databases_routes_honor_injected_manager(Backend::Postgres).await;
}

/// The `/api/databases` handlers operate on the *manager*, not a resolved
/// core, so they must resolve it through `request_manager` — honoring an
/// injected [`RequestDatabaseManager`] exactly like the `Db` extractor.
/// Before this contract they read `state.manager` directly, which made the
/// manager-plane routes silently ignore the composition: a split brain
/// where `/api/atoms` saw the injected manager but `/api/databases` saw
/// AppState's.
///
/// On SQLite the injected manager comes from a second, fully disjoint
/// `TestCtx`, so the test can also assert *negative* space: databases
/// listed/created through the injected manager must not exist in
/// AppState's. On Postgres both managers share one physical database (see
/// `run_injected_manager_resolution`), so the disjointness assertions are
/// SQLite-only; the per-manager in-memory active-database state (returned
/// by GET, mutated by PUT activate) keeps the injection observable there.
async fn run_databases_routes_honor_injected_manager(backend: Backend) {
    let is_postgres = matches!(backend, Backend::Postgres);
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };

    let (injected_manager, _disjoint_ctx) = second_manager(&ctx, is_postgres).await;
    let probe = injected_manager
        .create_database("injection-probe")
        .await
        .expect("create probe database in injected manager");
    injected_manager
        .set_active(&probe.id)
        .await
        .expect("activate probe database");
    let state_active_before = ctx.state.manager.active_id().expect("state active id");

    let app = init_app_with_injected_manager(&ctx, Arc::clone(&injected_manager)).await;

    // GET /api/databases reflects the injected manager: its database list
    // and its (per-manager, in-memory) active database.
    let req = actix_test::TestRequest::get()
        .uri("/api/databases")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "GET /api/databases should succeed");
    let body: Value = actix_test::read_body_json(resp).await;
    let listed: std::collections::HashSet<String> = body["databases"]
        .as_array()
        .expect("databases array")
        .iter()
        .filter_map(|d| d["id"].as_str().map(str::to_string))
        .collect();
    assert!(
        listed.contains(&probe.id),
        "GET /api/databases must list the injected manager's databases"
    );
    assert_eq!(
        body["active_id"], probe.id,
        "GET /api/databases must report the injected manager's active database"
    );
    assert_ne!(
        state_active_before, probe.id,
        "sanity: AppState's active database must differ from the probe"
    );
    if !is_postgres {
        let (state_dbs, _) = ctx
            .state
            .manager
            .list_databases()
            .await
            .expect("state list");
        assert!(
            !state_dbs.iter().any(|d| d.id == probe.id),
            "the probe database must not exist in AppState's manager"
        );
    }

    // POST /api/databases creates in the injected manager.
    let created_id = create_database(&app, &ctx, "created-through-injection").await;
    let (injected_dbs, _) = injected_manager
        .list_databases()
        .await
        .expect("injected list");
    assert!(
        injected_dbs.iter().any(|d| d.id == created_id),
        "POST /api/databases must create in the injected manager"
    );
    if !is_postgres {
        let (state_dbs, _) = ctx
            .state
            .manager
            .list_databases()
            .await
            .expect("state list");
        assert!(
            !state_dbs.iter().any(|d| d.id == created_id),
            "POST /api/databases must not create in AppState's manager"
        );
    }

    // PUT activate mutates the injected manager's active database and
    // leaves AppState's untouched — meaningful on both backends because
    // active state lives in each manager, not in shared storage.
    let req = actix_test::TestRequest::put()
        .uri(&format!("/api/databases/{}/activate", created_id))
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "PUT activate should succeed");
    assert_eq!(
        injected_manager.active_id().expect("injected active id"),
        created_id,
        "activate must switch the injected manager"
    );
    assert_eq!(
        ctx.state.manager.active_id().expect("state active id"),
        state_active_before,
        "activate must not touch AppState's manager"
    );
}

#[actix_web::test]
async fn pipeline_status_all_honors_injected_manager_sqlite() {
    run_pipeline_status_all_honors_injected_manager(Backend::Sqlite).await;
}

#[actix_web::test]
async fn pipeline_status_all_honors_injected_manager_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "pipeline_status_all_honors_injected_manager_postgres: skipping \
             (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_pipeline_status_all_honors_injected_manager(Backend::Postgres).await;
}

/// `GET /api/embeddings/status/all` fans out over every database of the
/// manager governing the request — one of the cross-database routes that
/// used to read `state.manager` directly. The same disjointness caveat as
/// [`run_databases_routes_honor_injected_manager`] applies: only SQLite can
/// construct truly disjoint managers, so the "not AppState's databases"
/// assertion is SQLite-only, while the probe-inclusion assertion runs on
/// both backends.
async fn run_pipeline_status_all_honors_injected_manager(backend: Backend) {
    let is_postgres = matches!(backend, Backend::Postgres);
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };

    let (injected_manager, _disjoint_ctx) = second_manager(&ctx, is_postgres).await;
    let probe = injected_manager
        .create_database("status-probe")
        .await
        .expect("create probe database in injected manager");
    // A database that exists only in AppState's manager (SQLite: the
    // managers are disjoint, so this is invisible to the injected one).
    let state_only = if is_postgres {
        None
    } else {
        Some(
            ctx.state
                .manager
                .create_database("state-only")
                .await
                .expect("create state-only database"),
        )
    };

    let app = init_app_with_injected_manager(&ctx, Arc::clone(&injected_manager)).await;

    let req = actix_test::TestRequest::get()
        .uri("/api/embeddings/status/all")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "GET status/all should succeed");
    let body: Value = actix_test::read_body_json(resp).await;
    let reported: std::collections::HashSet<String> = body["databases"]
        .as_array()
        .expect("databases array")
        .iter()
        .filter_map(|d| d["database"]["id"].as_str().map(str::to_string))
        .collect();
    assert!(
        reported.contains(&probe.id),
        "status/all must fan out over the injected manager's databases"
    );
    if let Some(state_only) = state_only {
        assert!(
            !reported.contains(&state_only.id),
            "status/all must not enumerate AppState's manager"
        );
    }
}

// ==================== Granular composition ====================

#[actix_web::test]
async fn granular_composition_sqlite() {
    run_granular_composition(Backend::Sqlite).await;
}

#[actix_web::test]
async fn granular_composition_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("granular_composition_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_granular_composition(Backend::Postgres).await;
}

/// `configure_app` is an all-in-one assembled from granular pieces —
/// `configure_public_routes`, `mcp_scope`, `api_scope` — exposed so a
/// caller can choose its own wrapping middleware per scope. This test
/// composes the pieces by hand, applying `BearerAuth`/`McpAuth` itself the
/// way `configure_app` does, and asserts the result behaves like the
/// all-in-one: public `/health` answers, `/api` admits a valid token and
/// rejects a missing one, `/mcp` 401s with OAuth discovery. If a route
/// migrates between pieces, this fails before any embedder-side
/// composition does. (It cannot detect a piece baking in auth of the same
/// scheme — both probes would still pass under a redundant double-wrap.)
async fn run_granular_composition(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(
        App::new()
            .configure(configure_public_routes(ctx.state.clone()))
            .service(mcp_scope(mcp_transport_for(&ctx)).wrap(McpAuth {
                state: ctx.state.clone(),
            }))
            .service(api_scope().wrap(BearerAuth {
                state: ctx.state.clone(),
            })),
    )
    .await;

    // Public piece: /health needs no credentials.
    let req = actix_test::TestRequest::get().uri("/health").to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "/health must be public");

    // API piece behind the test-applied BearerAuth: a valid token works...
    let req = actix_test::TestRequest::get()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200, "/api must work with a valid token");

    // ...and the wrap actually gates: no token is rejected with 401.
    let req = actix_test::TestRequest::get()
        .uri("/api/atoms")
        .to_request();
    let err = match actix_test::try_call_service(&app, req).await {
        Ok(resp) => panic!("/api must reject missing tokens, got {}", resp.status()),
        Err(err) => err,
    };
    assert_eq!(err.as_response_error().error_response().status(), 401);

    // MCP piece behind the test-applied McpAuth: unauthenticated requests
    // get the OAuth-discovery 401, same as the all-in-one.
    let req = actix_test::TestRequest::post().uri("/mcp").to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 401, "/mcp must reject missing tokens");
    assert!(
        resp.headers()
            .get("WWW-Authenticate")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("resource_metadata=")),
        "MCP 401 must point at OAuth discovery"
    );
}

// ==================== Injected event-channel resolution ====================

#[actix_web::test]
async fn injected_event_channel_sqlite() {
    run_injected_event_channel(Backend::Sqlite).await;
}

#[actix_web::test]
async fn injected_event_channel_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("injected_event_channel_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_injected_event_channel(Backend::Postgres).await;
}

/// `EventChannel` resolution contract for embedders: a layer composing
/// `configure_app` under its own middleware can install a
/// [`RequestEventChannel`] per request, and every request-driven event —
/// both the synchronous `AtomCreated` broadcast and the background pipeline
/// events spawned by the handler — lands on the injected channel, with
/// nothing leaking onto `AppState`'s process-wide channel.
async fn run_injected_event_channel(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };

    // Subscribe to both channels *before* any request so no event can slip
    // past (broadcast senders drop events sent while no receiver exists).
    let (injected_tx, mut injected_rx) = broadcast::channel::<ServerEvent>(64);
    let mut state_rx = ctx.state.event_tx.subscribe();

    let injected = RequestEventChannel(injected_tx);
    let app = actix_test::init_service(
        App::new()
            .wrap(from_fn(move |req: ServiceRequest, next: Next<_>| {
                let injected = injected.clone();
                async move {
                    req.extensions_mut().insert(injected);
                    next.call(req).await
                }
            }))
            .configure(configure_app(ctx.state.clone(), mcp_transport_for(&ctx))),
    )
    .await;

    let atom_id = post_atom(
        &app,
        &ctx,
        None,
        "gamma — events must ride the injected channel",
    )
    .await;

    // AtomCreated is sent inline by the handler; EmbeddingComplete comes
    // from the pipeline's background task via the on_event callback. Seeing
    // both proves the override covers direct sends *and* callbacks.
    await_broadcast_event(&mut injected_rx, |e| {
        e["type"] == "AtomCreated" && e["atom"]["id"] == atom_id.as_str()
    })
    .await;
    await_broadcast_event(&mut injected_rx, |e| {
        e["type"] == "EmbeddingComplete" && e["atom_id"] == atom_id.as_str()
    })
    .await;

    assert!(
        matches!(
            state_rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ),
        "request-driven events must not reach AppState's channel when one is injected"
    );
}

#[actix_web::test]
async fn event_channel_fallback_sqlite() {
    run_event_channel_fallback(Backend::Sqlite).await;
}

#[actix_web::test]
async fn event_channel_fallback_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!("event_channel_fallback_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)");
        return;
    }
    run_event_channel_fallback(Backend::Postgres).await;
}

/// With no middleware installed (the standalone server), the `EventChannel`
/// extractor falls back to `AppState`'s process-wide channel. The WS suite
/// relies on this implicitly; assert it once, explicitly, next to the
/// override test.
async fn run_event_channel_fallback(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    let mut state_rx = ctx.state.event_tx.subscribe();
    let atom_id = post_atom(
        &app,
        &ctx,
        None,
        "delta — events fall back to AppState's channel",
    )
    .await;

    await_broadcast_event(&mut state_rx, |e| {
        e["type"] == "AtomCreated" && e["atom"]["id"] == atom_id.as_str()
    })
    .await;
    await_broadcast_event(&mut state_rx, |e| {
        e["type"] == "EmbeddingComplete" && e["atom_id"] == atom_id.as_str()
    })
    .await;
}

/// Receive events from `rx` until `predicate` matches one (compared on its
/// JSON form, the same shape WS clients see), or panic after 15s.
async fn await_broadcast_event<F>(rx: &mut broadcast::Receiver<ServerEvent>, mut predicate: F)
where
    F: FnMut(&Value) -> bool,
{
    let deadline = Duration::from_secs(15);
    let stop_at = tokio::time::Instant::now() + deadline;
    loop {
        let remaining = stop_at.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("broadcast event predicate did not match within {deadline:?}");
        }
        let event = tokio::time::timeout(remaining, rx.recv())
            .await
            .expect("broadcast recv timeout")
            .expect("broadcast channel closed");
        let event = serde_json::to_value(&event).expect("serialize ServerEvent");
        if predicate(&event) {
            return;
        }
    }
}

#[actix_web::test]
async fn ws_streams_injected_event_channel_sqlite() {
    run_ws_streams_injected_event_channel(Backend::Sqlite).await;
}

#[actix_web::test]
async fn ws_streams_injected_event_channel_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "ws_streams_injected_event_channel_postgres: skipping \
             (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_ws_streams_injected_event_channel(Backend::Postgres).await;
}

/// The WebSocket handler honors the injected channel too: a WS client
/// connected to a composition that installs a [`RequestEventChannel`]
/// receives the events that request-driven handlers publish there. This is
/// the consumer half of the contract — without it, an embedder's WS clients
/// would be subscribed to a channel nobody sends to.
async fn run_ws_streams_injected_event_channel(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };

    let (injected_tx, _) = broadcast::channel::<ServerEvent>(64);
    let mut state_rx = ctx.state.event_tx.subscribe();
    let server = spawn_live_server_with_event_channel(&ctx, injected_tx).await;

    // Connect the WS first so it's subscribed before the POST fires events.
    let ws_url = format!(
        "{}/ws?token={}",
        server.base_url.replace("http://", "ws://"),
        ctx.token
    );
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .expect("ws upgrade should succeed");

    // POST over real HTTP. The middleware routes the handler's events into
    // the injected channel; the WS client must observe them there.
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/api/atoms", server.base_url))
        .bearer_auth(&ctx.token)
        .json(&json!({ "content": "epsilon — ws client rides the injected channel" }))
        .send()
        .await
        .expect("POST /api/atoms");
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.expect("parse atom response");
    let atom_id = body["id"].as_str().expect("atom id").to_string();

    collect_ws_event_until(&mut ws, Duration::from_secs(15), |e| {
        e["type"] == "EmbeddingComplete" && e["atom_id"] == atom_id.as_str()
    })
    .await;

    // The producer side never touched the process-wide channel either.
    assert!(
        matches!(
            state_rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ),
        "request-driven events must not reach AppState's channel when one is injected"
    );

    ws.send(tokio_tungstenite::tungstenite::Message::Close(None))
        .await
        .ok();
    server.stop().await;
}

/// Build a second [`DatabaseManager`] for injection tests.
///
/// SQLite: a fully disjoint `TestCtx` — its own registry, databases, and
/// settings — returned alongside the manager so its temp dir outlives the
/// test. Postgres: a second manager over the *same* physical database (the
/// backend shares one per process, so disjoint storage is not
/// constructible; see `run_injected_manager_resolution`), distinguishable
/// from AppState's only through per-manager in-memory state.
async fn second_manager(
    ctx: &TestCtx,
    is_postgres: bool,
) -> (Arc<DatabaseManager>, Option<TestCtx>) {
    if is_postgres {
        let url = std::env::var("ATOMIC_TEST_DATABASE_URL").expect("postgres url");
        let manager = Arc::new(
            DatabaseManager::new_postgres(ctx.data_dir(), &url)
                .await
                .expect("open second postgres manager"),
        );
        (manager, None)
    } else {
        let disjoint = TestCtx::new(Backend::Sqlite)
            .await
            .expect("sqlite ctx is always constructible");
        (Arc::clone(&disjoint.state.manager), Some(disjoint))
    }
}

/// Initialize an in-process app serving `configure_app`'s route table under
/// a middleware that installs `manager` as every request's
/// [`RequestDatabaseManager`] — the composition shape an embedder would use.
async fn init_app_with_injected_manager(
    ctx: &TestCtx,
    manager: Arc<DatabaseManager>,
) -> impl actix_web::dev::Service<
    actix_http::Request,
    Response = actix_web::dev::ServiceResponse<impl actix_web::body::MessageBody>,
    Error = actix_web::Error,
> {
    let injected = RequestDatabaseManager(manager);
    actix_test::init_service(
        App::new()
            .wrap(from_fn(move |req: ServiceRequest, next: Next<_>| {
                let injected = injected.clone();
                async move {
                    req.extensions_mut().insert(injected);
                    next.call(req).await
                }
            }))
            .configure(configure_app(ctx.state.clone(), mcp_transport_for(ctx))),
    )
    .await
}

/// POST /api/atoms, optionally into a specific database, returning the id.
async fn post_atom<S, B>(app: &S, ctx: &TestCtx, db_id: Option<&str>, content: &str) -> String
where
    S: actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse<B>,
        Error = actix_web::Error,
    >,
    B: actix_web::body::MessageBody,
{
    let mut req = actix_test::TestRequest::post()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "content": content }));
    if let Some(db_id) = db_id {
        req = req.insert_header(ctx.db_header(db_id));
    }
    let resp = actix_test::call_service(app, req.to_request()).await;
    assert_eq!(resp.status(), 201, "POST /api/atoms should return 201");
    let body: Value = actix_test::read_body_json(resp).await;
    body["id"]
        .as_str()
        .expect("created atom has id")
        .to_string()
}

/// POST /api/databases, returning the new database's id.
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
    assert_eq!(resp.status(), 201, "POST /api/databases should return 201");
    let body: Value = actix_test::read_body_json(resp).await;
    body["id"]
        .as_str()
        .expect("created database has id")
        .to_string()
}

/// GET /api/atoms (optionally scoped via the X-Atomic-Database header),
/// returning the set of atom ids in the listing.
async fn list_atom_ids<S, B>(
    app: &S,
    ctx: &TestCtx,
    db_id: Option<&str>,
) -> std::collections::HashSet<String>
where
    S: actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse<B>,
        Error = actix_web::Error,
    >,
    B: actix_web::body::MessageBody,
{
    let mut req = actix_test::TestRequest::get()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header());
    if let Some(db_id) = db_id {
        req = req.insert_header(ctx.db_header(db_id));
    }
    let resp = actix_test::call_service(app, req.to_request()).await;
    assert_eq!(resp.status(), 200, "GET /api/atoms should succeed");
    let body: Value = actix_test::read_body_json(resp).await;
    body["atoms"]
        .as_array()
        .expect("listing has atoms array")
        .iter()
        .filter_map(|a| a["id"].as_str().map(str::to_string))
        .collect()
}
