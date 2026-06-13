//! Shared infrastructure for atomic-server end-to-end tests.
//!
//! The same `Backend` switch used in atomic-core's pipeline tests runs each
//! suite against SQLite (always) and Postgres (when `ATOMIC_TEST_DATABASE_URL`
//! is set). Each `TestCtx` owns an `AppState` backed by the chosen store
//! plus a freshly minted API token; `test_app(&ctx)` produces an actix-web
//! `App` composed via `atomic_server::app::configure_app` — the exact route
//! table the production binary serves (public routes, `/mcp` behind McpAuth,
//! `/api` behind BearerAuth). `spawn_live_server(&ctx)` binds the same
//! composition to a real port for tests that need WebSocket upgrades or
//! concurrent HTTP load. The only difference from production is what the
//! caller wraps around it: `main.rs` adds CORS + compression, tests add
//! nothing.
//!
//! The wiremock-backed `MockAiServer` and the Postgres truncate helper live
//! in the workspace's `atomic-test-support` crate so atomic-core and
//! atomic-server share one implementation. This file owns only the pieces
//! tied to atomic-server's concrete `AppState` / route shape.

#![allow(dead_code)] // Helpers are per-test; not every test uses every helper.

use std::sync::Arc;
use std::time::Duration;

use actix_web::{web, App};
use atomic_core::DatabaseManager;
use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::broadcast;

use atomic_server::app::configure_app;
use atomic_server::event_channel::RequestEventChannel;
use atomic_server::export_jobs::ExportJobManager;
use atomic_server::log_buffer::LogBuffer;
use atomic_server::mcp::AtomicMcpTransport;
use atomic_server::state::{AppState, ServerEvent, SetupClaimLimiter};

// Re-export the shared mock + truncate helper so test files can keep using
// `support::MockAiServer` paths. (EMBED_DIM / EDGE_SIMILARITY_THRESHOLD are
// available via atomic_test_support directly for any future test that wants
// them.) `unused_imports` allowed because each integration-test binary
// compiles this module fresh and a few binaries don't reach into the mock.
#[allow(unused_imports)]
pub use atomic_test_support::{truncate_postgres_for_test, MockAiServer, MockUrlServer};

// ==================== Backend switch ====================

pub enum Backend {
    Sqlite,
    Postgres,
}

impl Backend {
    pub fn name(&self) -> &'static str {
        match self {
            Backend::Sqlite => "sqlite",
            Backend::Postgres => "postgres",
        }
    }
}

/// Test context — owns the temp dir (so the SQLite DB stays alive), the
/// AppState, and the raw bearer token. Drop order matters: `_temp` lives
/// strictly longer than `state` because handlers may still be flushing.
pub struct TestCtx {
    _temp: Option<TempDir>,
    pub state: web::Data<AppState>,
    pub token: String,
    pub mock: Arc<MockAiServer>,
}

/// Build options for `TestCtx::new_with`. Defaults match the values used
/// by `TestCtx::new` so the no-knobs path stays one line.
pub struct TestCtxOptions {
    /// When true (the default), mints an initial API token and seeds the
    /// `ctx.token` field. Setup tests pass `false` so the instance is
    /// genuinely unclaimed when the test starts.
    pub mint_initial_token: bool,
    /// Sets `AppState::public_url`. OAuth endpoints early-return 404 when
    /// this is `None`; the OAuth tests pass `Some(<server base url>)`.
    pub public_url: Option<String>,
    /// Sets `AppState::dangerously_skip_setup_token`. Setup token tests
    /// flip this to exercise the token-required branch.
    pub dangerously_skip_setup_token: bool,
    /// Sets `AppState::setup_token`. Tests that exercise the setup token
    /// branch pass a known value here.
    pub setup_token: Option<atomic_server::state::SetupToken>,
}

impl Default for TestCtxOptions {
    fn default() -> Self {
        Self {
            mint_initial_token: true,
            public_url: None,
            dangerously_skip_setup_token: true,
            setup_token: None,
        }
    }
}

impl TestCtx {
    /// Build a fresh test context on the chosen backend. Returns `None` when
    /// the Postgres URL is unset so individual tests can skip cleanly rather
    /// than failing on a missing env var.
    pub async fn new(backend: Backend) -> Option<Self> {
        Self::new_with(backend, TestCtxOptions::default()).await
    }

    /// Like [`new`] but with explicit options so OAuth/setup tests can
    /// flip `public_url`, `mint_initial_token`, and the setup-token gate
    /// without forking the whole constructor.
    pub async fn new_with(backend: Backend, opts: TestCtxOptions) -> Option<Self> {
        let mock = Arc::new(MockAiServer::start().await);

        let (manager, temp) = match backend {
            Backend::Sqlite => {
                let dir = TempDir::new().expect("create tempdir");
                let manager =
                    Arc::new(DatabaseManager::new(dir.path()).expect("open sqlite manager"));
                (manager, Some(dir))
            }
            Backend::Postgres => {
                let url = std::env::var("ATOMIC_TEST_DATABASE_URL").ok()?;
                truncate_postgres_for_test(&url).await;
                let dir = TempDir::new().expect("create tempdir");
                let manager = Arc::new(
                    DatabaseManager::new_postgres(dir.path(), &url)
                        .await
                        .expect("open postgres manager"),
                );
                // Tempdir holds the export_jobs work tree; the manager itself
                // ignores it for Postgres backends.
                (manager, Some(dir))
            }
        };

        // Configure the active core to use the mock AI provider so the
        // embedding + tagging pipeline runs end-to-end during tests.
        let core = manager.active_core().await.expect("active core");
        for (k, v) in [
            ("provider", "openai_compat"),
            ("openai_compat_base_url", mock.base_url().as_str()),
            ("openai_compat_api_key", "test-key"),
            ("openai_compat_embedding_model", "mock-embed"),
            ("openai_compat_llm_model", "mock-llm"),
            ("openai_compat_embedding_dimension", "1536"),
            ("auto_tagging_enabled", "true"),
        ] {
            core.set_setting(k, v).await.expect("seed test setting");
        }
        core.configure_autotag_targets(&["Topics".to_string()], &[])
            .await
            .expect("configure autotag targets");

        let raw_token = if opts.mint_initial_token {
            let (_info, raw_token) = core
                .create_api_token("e2e-test")
                .await
                .expect("mint api token");
            raw_token
        } else {
            // Setup tests need a *truly* unclaimed instance — `claim_instance`
            // refuses to run if any token exists. The `token` field is left
            // empty; tests that need auth after claiming should mint a token
            // through the claim flow itself.
            String::new()
        };

        let temp_for_exports = temp
            .as_ref()
            .map(|d| d.path().to_path_buf())
            .unwrap_or_else(|| std::env::temp_dir().join("atomic-e2e-exports"));
        let (event_tx, _) = broadcast::channel(64);
        let state = web::Data::new(AppState {
            manager,
            event_tx,
            public_url: opts.public_url,
            log_buffer: LogBuffer::new(16),
            export_jobs: ExportJobManager::for_tests(temp_for_exports.join("exports")),
            setup_token: opts.setup_token,
            dangerously_skip_setup_token: opts.dangerously_skip_setup_token,
            setup_claim_lock: tokio::sync::Mutex::new(()),
            setup_claim_limiter: SetupClaimLimiter::new(),
        });

        Some(TestCtx {
            _temp: temp,
            state,
            token: raw_token,
            mock,
        })
    }

    /// Path to this context's data directory — registry + database files on
    /// SQLite, export scratch space on Postgres. Lets tests open a second
    /// `DatabaseManager` over the same storage as `state.manager`.
    pub fn data_dir(&self) -> &std::path::Path {
        self._temp
            .as_ref()
            .map(|d| d.path())
            .expect("TestCtx always owns a temp dir")
    }

    pub fn auth_header(&self) -> (&'static str, String) {
        ("Authorization", format!("Bearer {}", self.token))
    }

    pub fn db_header(&self, db_id: &str) -> (&'static str, String) {
        ("X-Atomic-Database", db_id.to_string())
    }
}

/// Construct the MCP transport for a test context. Mirrors `main.rs`: built
/// once per server (outside any worker factory) so all workers share one
/// session manager. Public so tests that compose their own `App` (e.g. with
/// extra middleware around `configure_app`) reuse the same wiring.
pub fn mcp_transport_for(ctx: &TestCtx) -> AtomicMcpTransport {
    AtomicMcpTransport::new(
        Arc::clone(&ctx.state.manager),
        ctx.state.event_tx.clone(),
        Duration::from_secs(30),
    )
}

/// Build an in-process actix-web `App` composed from
/// `atomic_server::app::configure_app` — the same function `main.rs` uses,
/// so the test app serves the production route table verbatim (public
/// routes, `/mcp` behind McpAuth, `/api` behind BearerAuth). Only the
/// deployment middleware (CORS, compression) is absent, because that's
/// layered by the caller in production and tests layer nothing.
pub fn test_app(
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
    App::new().configure(configure_app(ctx.state.clone(), mcp_transport_for(ctx)))
}

// ==================== Real-port test server ====================

/// Handle to a `HttpServer` running on an ephemeral port. Drop the handle (or
/// call [`stop`](LiveServer::stop)) to shut it down.
pub struct LiveServer {
    pub base_url: String,
    handle: actix_web::dev::ServerHandle,
}

impl LiveServer {
    pub async fn stop(self) {
        self.handle.stop(false).await;
    }
}

/// Start a real `HttpServer` on `127.0.0.1:0` serving the production route
/// table via `atomic_server::app::configure_app` — exactly as `main.rs`
/// composes it, minus the CORS/compression middleware the binary layers on
/// top. The returned `base_url` points at the bound port; the server runs
/// on its own tokio task until the handle is stopped.
///
/// Used by the WebSocket, MCP, OAuth/setup, and concurrent-storm suites
/// that need a real TCP listener — `actix_web::test::init_service` is
/// in-process only and can't satisfy `actix-ws`'s upgrade response or
/// model real concurrent HTTP load.
pub async fn spawn_live_server(ctx: &TestCtx) -> LiveServer {
    use actix_web::{App, HttpServer};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let base_url = format!("http://{}", addr);

    // MCP transport must be constructed once and cloned into each worker so
    // the LocalSessionManager is shared. Mirrors the wiring in main.rs.
    let mcp_transport = mcp_transport_for(ctx);

    let state_for_factory = ctx.state.clone();
    let server = HttpServer::new(move || {
        App::new().configure(configure_app(
            state_for_factory.clone(),
            mcp_transport.clone(),
        ))
    })
    .workers(1)
    .listen(listener)
    .expect("attach listener")
    .run();

    let handle = server.handle();
    actix_web::rt::spawn(server);

    LiveServer { base_url, handle }
}

/// Like [`spawn_live_server`], but wraps the composed routes in a middleware
/// that installs `tx` as each request's
/// [`RequestEventChannel`] — the live equivalent of an embedder overriding
/// the event channel per request. Used to prove, over a real socket, that
/// the WS handler subscribes to the injected channel and that route
/// handlers publish into it.
pub async fn spawn_live_server_with_event_channel(
    ctx: &TestCtx,
    tx: broadcast::Sender<ServerEvent>,
) -> LiveServer {
    use actix_web::dev::ServiceRequest;
    use actix_web::middleware::{from_fn, Next};
    use actix_web::{App, HttpMessage, HttpServer};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let base_url = format!("http://{}", addr);

    let mcp_transport = mcp_transport_for(ctx);
    let state_for_factory = ctx.state.clone();
    let server = HttpServer::new(move || {
        let injected = RequestEventChannel(tx.clone());
        App::new()
            .wrap(from_fn(move |req: ServiceRequest, next: Next<_>| {
                let injected = injected.clone();
                async move {
                    req.extensions_mut().insert(injected);
                    next.call(req).await
                }
            }))
            .configure(configure_app(
                state_for_factory.clone(),
                mcp_transport.clone(),
            ))
    })
    .workers(1)
    .listen(listener)
    .expect("attach listener")
    .run();

    let handle = server.handle();
    actix_web::rt::spawn(server);

    LiveServer { base_url, handle }
}

// ==================== Pipeline poller ====================

// ==================== WS event collector ====================

/// Collect WS events from `ws` until `predicate(&event)` returns true, or
/// `deadline` elapses. Skips non-text frames (ping/pong/binary). Panics on a
/// clean Close (server hanging up mid-test is a real failure signal). Returns
/// the matched event so callers can assert on it further.
///
/// Shared between the WS pipeline test and the chat tests because both wait
/// for "some predicate over a stream of JSON frames" — putting the loop in
/// one place keeps the test bodies small enough to read end-to-end without
/// scrolling.
pub async fn collect_ws_event_until<F, S>(
    ws: &mut S,
    deadline: std::time::Duration,
    mut predicate: F,
) -> serde_json::Value
where
    F: FnMut(&serde_json::Value) -> bool,
    S: futures_util::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let stop_at = tokio::time::Instant::now() + deadline;
    loop {
        let remaining = stop_at.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("ws predicate did not match within {deadline:?}");
        }
        let msg = tokio::time::timeout(remaining, ws.next())
            .await
            .expect("ws recv timeout")
            .expect("ws stream ended")
            .expect("ws frame");
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {
                continue
            }
            Message::Close(_) => panic!("server closed the ws connection mid-test"),
        };
        let event: serde_json::Value = serde_json::from_str(&text).expect("ws frame is JSON");
        if predicate(&event) {
            return event;
        }
    }
}

/// Poll `GET /api/atoms/{id}` until `embedding_status` reaches a terminal
/// state (`complete` or `failed`). Returns the parsed atom body. The mock
/// embedder responds instantly, but the pipeline runs on a background tokio
/// task — without polling, tests would race the response.
pub async fn poll_until_embedding_done<S, B>(
    app: &S,
    auth: (&'static str, String),
    atom_id: &str,
) -> Value
where
    S: actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse<B>,
        Error = actix_web::Error,
    >,
    B: actix_web::body::MessageBody,
{
    poll_until_atom_status(app, auth, atom_id, "embedding_status").await
}

/// Wait until the atom's tagging stage hits a terminal state. Auto-tagging
/// runs *after* embedding completes, so checks against the `tags` array
/// must gate on this rather than `embedding_status` — otherwise the test
/// races the background tagger and sometimes sees an empty `tags` list.
pub async fn poll_until_tagging_done<S, B>(
    app: &S,
    auth: (&'static str, String),
    atom_id: &str,
) -> Value
where
    S: actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse<B>,
        Error = actix_web::Error,
    >,
    B: actix_web::body::MessageBody,
{
    poll_until_atom_status(app, auth, atom_id, "tagging_status").await
}

async fn poll_until_atom_status<S, B>(
    app: &S,
    auth: (&'static str, String),
    atom_id: &str,
    field: &'static str,
) -> Value
where
    S: actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse<B>,
        Error = actix_web::Error,
    >,
    B: actix_web::body::MessageBody,
{
    use actix_web::test as actix_test;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let req = actix_test::TestRequest::get()
            .uri(&format!("/api/atoms/{}", atom_id))
            .insert_header(auth.clone())
            .to_request();
        let resp = actix_test::call_service(app, req).await;
        assert_eq!(resp.status(), 200, "atom should exist while polling");
        let body: Value = actix_test::read_body_json(resp).await;
        let status = body[field].as_str().unwrap_or("");
        // `skipped` is a terminal state too — e.g. when an atom is too short
        // to tag, the pipeline marks tagging skipped rather than complete.
        if matches!(status, "complete" | "failed" | "skipped") {
            return body;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "{field} did not reach terminal state for {atom_id} within 15s; \
                 last status = {status:?}"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}
