//! End-to-end tests for the composed cloud server.
//!
//! Each test spawns the real composition — `configure_cloud_app` on an
//! ephemeral port, exactly as the `atomic-cloud serve` binary wires it —
//! provisions accounts against the test cluster, and drives them with
//! `reqwest` (setting the `Host` header explicitly, e.g.
//! `alpha.cloudtest.local` against `127.0.0.1`) and `tokio-tungstenite` for
//! WebSocket assertions. Tenant provider settings point at the shared
//! `MockAiServer`, so atom-creation pipelines never reach real providers.
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command. The one exception is the fallback
//! fail-closed guard test, which exercises the SQLite scratch state and
//! needs no cluster.

mod support;

use std::sync::Arc;
use std::time::Duration;

use actix_web::{App, HttpServer};
use atomic_cloud::{
    cloud_plane_guard, configure_cloud_app, create_session, delete_account, issue_token,
    list_hinted_accounts, provision_account, set_active_provider, tenant_schema_target,
    upsert_credentials, AccountCache, AccountCacheConfig, AccountPlane, AccountPlaneConfig,
    ChatStreamLimiter, CloudAuth, ClusterConfig, ControlPlane, CredentialOrigin, FallbackAppState,
    ManagedKeys, NewAccount, NewCredentials, Provider, QuotaBilling, Readiness, SecretKey,
    SpaServer, TenantPlane, TokenScope, DEFAULT_CHAT_STREAMS_PER_ACCOUNT, SESSION_COOKIE,
};
use atomic_core::DatabaseManager;
use atomic_test_support::MockAiServer;
use futures_util::StreamExt;
use reqwest::header::HOST;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use sqlx::{Connection, PgConnection};
use support::with_control_db;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

/// Base domain the composition is configured with; accounts are addressed
/// as `<subdomain>.cloudtest.local` while TCP goes to `127.0.0.1`.
const BASE_DOMAIN: &str = "cloudtest.local";

/// How long to wait for asynchronous outcomes (pipeline completion, WS
/// frames) before failing the test.
const EVENT_DEADLINE: Duration = Duration::from_secs(15);

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A provisioned account plus the credential the test drives it with.
struct Tenant {
    account_id: String,
    subdomain: String,
    db_name: String,
    token: String,
}

/// The composed cloud server on an ephemeral port, plus handles to
/// everything a test needs to provision tenants and inspect the cache.
struct CloudHarness {
    control: ControlPlane,
    cluster: ClusterConfig,
    cache: Arc<AccountCache>,
    mock: MockAiServer,
    client: reqwest::Client,
    port: u16,
    base_url: String,
    handle: actix_web::dev::ServerHandle,
    /// Owns the scratch directory behind the inert fallback `AppState`;
    /// must outlive the server.
    _fallback: FallbackAppState,
    /// Owns the fixture `dist/` the SPA fallback serves; must outlive the
    /// server (the SPA reads assets from disk on each request).
    _spa_dir: tempfile::TempDir,
}

impl CloudHarness {
    /// Spawn the composition exactly as `atomic-cloud serve` wires it:
    /// migrated control plane, `AccountCache` (with the given config so the
    /// eviction test can shrink it), `CloudAuth`, fallback state, one worker
    /// on `127.0.0.1:0`.
    async fn spawn(control_url: &str, cache_config: AccountCacheConfig) -> Self {
        let control = ControlPlane::connect(
            control_url,
            atomic_cloud::control_plane::DEFAULT_CONTROL_POOL_MAX_CONNECTIONS,
        )
        .await
        .expect("connect control plane");
        control.initialize().await.expect("migrate control plane");
        let cluster = ClusterConfig {
            cluster_id: "test-cluster-1".to_string(),
            cluster_url: std::env::var("ATOMIC_TEST_DATABASE_URL")
                .expect("with_control_db verified ATOMIC_TEST_DATABASE_URL"),
        };
        let mock = MockAiServer::start().await;
        let cache = Arc::new(AccountCache::new(
            control.clone(),
            cluster.clone(),
            support::test_vault(),
            cache_config,
        ));
        let auth = CloudAuth::new(control.clone(), Arc::clone(&cache), BASE_DOMAIN);
        // This suite never drives the account plane (tests/account_plane.rs
        // owns that); a capturing sender keeps any accidental traffic from
        // sending mail.
        let account_plane = AccountPlane::new(
            control.clone(),
            cluster.clone(),
            ManagedKeys::Disabled,
            Arc::new(support::CapturingSender::default()),
            AccountPlaneConfig::new(BASE_DOMAIN),
        )
        .expect("build account plane");
        let tenant_plane = TenantPlane::new(
            control.clone(),
            cluster.clone(),
            ManagedKeys::Disabled,
            support::test_vault(),
            Arc::clone(&cache),
        );
        let fallback = FallbackAppState::build().expect("build fallback state");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local addr").port();
        let state = fallback.data();
        let control_for_app = control.clone();
        let chat_streams = ChatStreamLimiter::new(DEFAULT_CHAT_STREAMS_PER_ACCOUNT);
        // This harness runs no fleet gate; the deploy-gating suite owns
        // readiness behavior.
        let readiness = Readiness::ready(control.clone());
        let quota_billing = QuotaBilling::for_tests(control.clone(), BASE_DOMAIN)
            .await
            .expect("plans");
        let oauth_plane = atomic_cloud::OAuthPlane::new(
            control.clone(),
            BASE_DOMAIN,
            "http",
            format!("http://app.{BASE_DOMAIN}"),
        );
        let mcp_transport = fallback.mcp_transport(atomic_cloud::DEFAULT_MCP_SSE_KEEP_ALIVE);

        // A fixture `dist/` so the composed server serves the account-plane
        // SPA fallback exactly as `serve` wires it — base-domain meta and all.
        let spa_dir = tempfile::tempdir().expect("spa fixture dir");
        std::fs::write(
            spa_dir.path().join("index.html"),
            r#"<!doctype html><html><head>
<meta name="atomic-cloud-base-domain" content="__ATOMIC_CLOUD_BASE_DOMAIN__" />
</head><body><div id="root"></div></body></html>"#,
        )
        .expect("write fixture index.html");
        let spa = SpaServer::load(spa_dir.path(), BASE_DOMAIN)
            .await
            .expect("load fixture SPA");

        let server = HttpServer::new(move || {
            App::new()
                .configure(configure_cloud_app(
                    state.clone(),
                    auth.clone(),
                    account_plane.clone(),
                    tenant_plane.clone(),
                    oauth_plane.clone(),
                    mcp_transport.clone(),
                    control_for_app.clone(),
                    chat_streams.clone(),
                    readiness.clone(),
                    quota_billing.clone(),
                    Some(spa.clone()),
                ))
                // Mirrors `serve`: the CORS edge answers extension/local-app
                // preflights before CloudAuth (see extension_cors_preflight).
                .wrap(atomic_server::cors::build_cors(None))
        })
        .workers(1)
        .listen(listener)
        .expect("attach listener")
        .run();
        let handle = server.handle();
        actix_web::rt::spawn(server);

        CloudHarness {
            control,
            cluster,
            cache,
            mock,
            client: reqwest::Client::new(),
            port,
            base_url: format!("http://127.0.0.1:{port}"),
            handle,
            _fallback: fallback,
            _spa_dir: spa_dir,
        }
    }

    async fn stop(self) {
        self.handle.stop(false).await;
    }

    /// Provision an account, store BYOK provider credentials in the control
    /// plane pointing at the mock AI server (the cache resolves provider
    /// config from the control plane only — tenant settings can't select
    /// providers in cloud), seed the non-provider AI settings in the tenant
    /// database, and issue an account-scope token.
    async fn provision(&self, subdomain: &str) -> Tenant {
        let account = provision_account(
            &self.control,
            &self.cluster,
            &ManagedKeys::Disabled,
            NewAccount {
                email: format!("{subdomain}@example.com"),
                subdomain: subdomain.to_string(),
            },
        )
        .await
        .expect("provision account");

        let vault = support::test_vault();
        upsert_credentials(
            &self.control,
            vault.as_ref(),
            &account.account_id,
            NewCredentials {
                provider: Provider::OpenAiCompat,
                origin: CredentialOrigin::User,
                api_key: SecretKey::new("test-key".to_string()),
                external_key_id: None,
                model_config: json!({
                    "embedding_model": "mock-embed",
                    "llm_model": "mock-llm",
                    "openai_compat_base_url": self.mock.base_url(),
                    "embedding_dimension": 1536,
                }),
            },
        )
        .await
        .expect("store mock provider credentials");
        set_active_provider(
            &self.control,
            &account.account_id,
            Some((Provider::OpenAiCompat, CredentialOrigin::User)),
        )
        .await
        .expect("activate mock provider credentials");

        // Non-provider AI settings still live in the tenant database.
        let tenant_url = self
            .cluster
            .tenant_db_url(&account.db_name)
            .expect("tenant url");
        let manager = DatabaseManager::new_postgres(".", &tenant_url)
            .await
            .expect("open tenant manager");
        let core = manager.active_core().await.expect("active core");
        core.set_setting("auto_tagging_enabled", "true")
            .await
            .expect("seed tenant setting");
        core.configure_autotag_targets(&["Topics".to_string()], &[])
            .await
            .expect("configure autotag targets");
        drop(manager);

        let token = issue_token(
            &self.control,
            &account.account_id,
            TokenScope::Account,
            None,
            "e2e",
        )
        .await
        .expect("issue account token");

        Tenant {
            account_id: account.account_id,
            subdomain: subdomain.to_string(),
            db_name: account.db_name,
            token,
        }
    }

    /// Request builder addressed at `subdomain.<BASE_DOMAIN>` (via explicit
    /// `Host` header) over the loopback listener. Caller attaches auth.
    fn api(&self, method: Method, subdomain: &str, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base_url))
            .header(HOST, format!("{subdomain}.{BASE_DOMAIN}"))
    }

    async fn create_atom(&self, tenant: &Tenant, content: &str) -> Value {
        let resp = self
            .api(Method::POST, &tenant.subdomain, "/api/atoms")
            .bearer_auth(&tenant.token)
            .json(&json!({ "content": content }))
            .send()
            .await
            .expect("send create atom");
        assert_eq!(resp.status(), StatusCode::CREATED, "create atom");
        resp.json().await.expect("atom json")
    }

    async fn list_atoms(&self, tenant: &Tenant) -> Value {
        let resp = self
            .api(Method::GET, &tenant.subdomain, "/api/atoms")
            .bearer_auth(&tenant.token)
            .send()
            .await
            .expect("send list atoms");
        assert_eq!(resp.status(), StatusCode::OK, "list atoms");
        resp.json().await.expect("atoms json")
    }

    /// Poll the atom until its embedding pipeline reaches a terminal state,
    /// so a tenant's background work is provably finished before the test
    /// asserts on another tenant's stream.
    async fn poll_pipeline_done(&self, tenant: &Tenant, atom_id: &str) {
        let deadline = std::time::Instant::now() + EVENT_DEADLINE;
        loop {
            let resp = self
                .api(
                    Method::GET,
                    &tenant.subdomain,
                    &format!("/api/atoms/{atom_id}"),
                )
                .bearer_auth(&tenant.token)
                .send()
                .await
                .expect("send get atom");
            assert_eq!(resp.status(), StatusCode::OK, "atom exists while polling");
            let body: Value = resp.json().await.expect("atom json");
            let status = body["embedding_status"].as_str().unwrap_or("");
            if matches!(status, "complete" | "failed" | "skipped") {
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!("pipeline for {atom_id} not terminal in {EVENT_DEADLINE:?}: {status:?}");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Open the cloud `/ws` route as `subdomain`'s tenant: loopback TCP,
    /// explicit `Host`, bearer auth in the upgrade request (the cloud route
    /// has no `?token=` — CloudAuth is the authenticator).
    async fn ws_connect(&self, tenant: &Tenant) -> WsStream {
        let mut request = format!("ws://127.0.0.1:{}/ws", self.port)
            .into_client_request()
            .expect("ws request");
        let headers = request.headers_mut();
        headers.insert(
            "Host",
            format!("{}.{BASE_DOMAIN}", tenant.subdomain)
                .parse()
                .expect("host header"),
        );
        headers.insert(
            "Authorization",
            format!("Bearer {}", tenant.token)
                .parse()
                .expect("auth header"),
        );
        let (ws, _resp) = tokio_tungstenite::connect_async(request)
            .await
            .expect("ws connect");
        ws
    }
}

/// Read text frames until `predicate` matches one, returning every frame
/// seen (matched frame last). Panics when `deadline` elapses or the server
/// closes the socket.
async fn collect_until<F>(ws: &mut WsStream, deadline: Duration, predicate: F) -> Vec<Value>
where
    F: Fn(&Value) -> bool,
{
    let stop_at = tokio::time::Instant::now() + deadline;
    let mut seen = Vec::new();
    loop {
        let remaining = stop_at.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("ws predicate not matched within {deadline:?}; saw {seen:?}");
        }
        let msg = tokio::time::timeout(remaining, ws.next())
            .await
            .unwrap_or_else(|_| {
                panic!("ws predicate not matched within {deadline:?}; saw {seen:?}")
            })
            .expect("ws stream ended")
            .expect("ws frame");
        match msg {
            Message::Text(t) => {
                let event: Value = serde_json::from_str(&t.to_string()).expect("ws frame is JSON");
                let matched = predicate(&event);
                seen.push(event);
                if matched {
                    return seen;
                }
            }
            Message::Close(_) => panic!("server closed the ws connection mid-test"),
            _ => continue,
        }
    }
}

/// Wait for the server to terminate the WebSocket, draining any straggler
/// frames. Accepts every shape a severed actix-ws connection produces at
/// the client: a Close frame, a clean stream end, or a reset without a
/// closing handshake (the forwarding task dropping its `Session` ends the
/// response body at the TCP level). Panics when `deadline` elapses first.
async fn await_ws_close(ws: &mut WsStream, deadline: Duration) {
    let stop_at = tokio::time::Instant::now() + deadline;
    loop {
        let remaining = stop_at.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("ws not closed within {deadline:?}");
        }
        match tokio::time::timeout(remaining, ws.next()).await {
            Err(_elapsed) => panic!("ws not closed within {deadline:?}"),
            Ok(None) => return,
            Ok(Some(Err(_))) => return,
            Ok(Some(Ok(Message::Close(_)))) => return,
            Ok(Some(Ok(_))) => continue,
        }
    }
}

/// Whether `db_name` exists on the test cluster.
async fn database_exists(db_name: &str) -> bool {
    let base_url = std::env::var("ATOMIC_TEST_DATABASE_URL").expect("env");
    let mut conn = PgConnection::connect(&base_url).await.expect("connect");
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(db_name)
            .fetch_one(&mut conn)
            .await
            .expect("query pg_database");
    let _ = conn.close().await;
    exists
}

fn atom_ids(listing: &Value) -> Vec<&str> {
    listing["atoms"]
        .as_array()
        .expect("atoms array")
        .iter()
        .map(|a| a["id"].as_str().expect("atom id"))
        .collect()
}

// ==================== Tests ====================

/// An atom created on alpha appears in alpha's listing and never in bravo's.
#[actix_web::test]
async fn tenant_isolation_atom_listing() {
    with_control_db("tenant_isolation_atom_listing", |url| async move {
        let h = CloudHarness::spawn(&url, AccountCacheConfig::default()).await;
        let alpha = h.provision("alpha").await;
        let bravo = h.provision("bravo").await;

        let atom = h
            .create_atom(&alpha, "Alpha's note about Rust workspaces.")
            .await;
        let atom_id = atom["id"].as_str().expect("atom id").to_string();

        let alpha_listing = h.list_atoms(&alpha).await;
        assert!(
            atom_ids(&alpha_listing).contains(&atom_id.as_str()),
            "alpha must see its own atom"
        );

        let bravo_listing = h.list_atoms(&bravo).await;
        assert_eq!(
            bravo_listing["total_count"], 0,
            "bravo's tenant database must be empty"
        );
        assert!(
            atom_ids(&bravo_listing).is_empty(),
            "alpha's atom must never appear in bravo's listing"
        );

        h.stop().await;
    })
    .await;
}

/// Chokepoint 1: a database-scoped token naming another knowledge base via
/// `X-Atomic-Database` is rejected (403); the same token without the header
/// reads its allowed KB.
#[actix_web::test]
async fn database_scoped_token_chokepoint() {
    with_control_db("database_scoped_token_chokepoint", |url| async move {
        let h = CloudHarness::spawn(&url, AccountCacheConfig::default()).await;
        let alpha = h.provision("alpha").await;

        // Content in the default KB, and a second KB to (fail to) reach.
        let atom = h.create_atom(&alpha, "Default-KB note.").await;
        let atom_id = atom["id"].as_str().expect("atom id").to_string();
        let resp = h
            .api(Method::POST, &alpha.subdomain, "/api/databases")
            .bearer_auth(&alpha.token)
            .json(&json!({ "name": "Second" }))
            .send()
            .await
            .expect("create second KB");
        assert_eq!(resp.status(), StatusCode::CREATED);
        let second: Value = resp.json().await.expect("database json");
        let second_id = second["id"].as_str().expect("db id").to_string();

        let scoped = issue_token(
            &h.control,
            &alpha.account_id,
            TokenScope::Database,
            Some("default"),
            "kb-pinned",
        )
        .await
        .expect("issue database-scoped token");

        // Naming the other KB explicitly → 403, before any handler runs.
        let resp = h
            .api(Method::GET, &alpha.subdomain, "/api/atoms")
            .bearer_auth(&scoped)
            .header("X-Atomic-Database", &second_id)
            .send()
            .await
            .expect("send scoped request");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body: Value = resp.json().await.expect("denial json");
        assert_eq!(body["error"], "database_forbidden");

        // No header → pinned to the credential's KB, which has the atom.
        let resp = h
            .api(Method::GET, &alpha.subdomain, "/api/atoms")
            .bearer_auth(&scoped)
            .send()
            .await
            .expect("send scoped request");
        assert_eq!(resp.status(), StatusCode::OK);
        let listing: Value = resp.json().await.expect("atoms json");
        assert!(
            atom_ids(&listing).contains(&atom_id.as_str()),
            "scoped token must read its allowed KB"
        );

        h.stop().await;
    })
    .await;
}

/// Chokepoint 2 (plan decision 2026-06-09): alpha's perfectly valid
/// credentials presented on bravo's subdomain verify nothing — token and
/// session both 401.
#[actix_web::test]
async fn cross_tenant_credentials_rejected() {
    with_control_db("cross_tenant_credentials_rejected_e2e", |url| async move {
        let h = CloudHarness::spawn(&url, AccountCacheConfig::default()).await;
        let alpha = h.provision("alpha").await;
        let bravo = h.provision("bravo").await;

        // Sanity: the token works where it belongs.
        let resp = h
            .api(Method::GET, &alpha.subdomain, "/api/atoms")
            .bearer_auth(&alpha.token)
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), StatusCode::OK);

        // Same token on bravo's subdomain → 401.
        let resp = h
            .api(Method::GET, &bravo.subdomain, "/api/atoms")
            .bearer_auth(&alpha.token)
            .send()
            .await
            .expect("send");
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "token must not cross tenants"
        );

        // Sessions cross subdomains by design (cookie domain is the base);
        // the account-scoped verification is what isolates tenants.
        let session = create_session(
            &h.control,
            &alpha.account_id,
            Duration::from_secs(3600),
            None,
            None,
        )
        .await
        .expect("create session");
        let resp = h
            .api(Method::GET, &bravo.subdomain, "/api/atoms")
            .header("Cookie", format!("{SESSION_COOKIE}={session}"))
            .send()
            .await
            .expect("send");
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "session must not cross tenants"
        );

        h.stop().await;
    })
    .await;
}

/// Routing edges: unknown subdomain → 404 (credential or not); a valid
/// subdomain without credentials → 401; `/health` is public; and the
/// self-hosted token plane (`/api/auth/*`) is unrouted in cloud — proving
/// `cloud_plane_guard` is wired into the live composition.
/// The CORS edge for browser-extension clients. The web clipper ships from
/// the Chrome Web Store without `host_permissions` (activeTab-only), so its
/// fetches are ordinary CORS-governed requests: the browser preflights them
/// with NO Authorization header. The CORS middleware must therefore answer
/// preflights *before* CloudAuth (which would 401 them), allow extension
/// origins without credentials support (the session-cookie plane stays
/// same-origin), and give foreign web origins nothing.
#[actix_web::test]
async fn extension_cors_preflight() {
    with_control_db("extension_cors_preflight", |url| async move {
        let h = CloudHarness::spawn(&url, AccountCacheConfig::default()).await;
        let alpha = h.provision("alpha").await;

        const EXT_ORIGIN: &str = "chrome-extension://bknijbafnefbaklndpglcmlhaglikccf";

        // Preflight, unauthenticated, exactly as a browser sends it.
        let resp = h
            .api(Method::OPTIONS, &alpha.subdomain, "/api/atoms")
            .header("Origin", EXT_ORIGIN)
            .header("Access-Control-Request-Method", "GET")
            .header(
                "Access-Control-Request-Headers",
                "authorization,content-type,x-atomic-database",
            )
            .send()
            .await
            .expect("send preflight");
        assert!(
            resp.status().is_success(),
            "preflight must be answered by the CORS edge, not CloudAuth: {}",
            resp.status()
        );
        assert_eq!(
            resp.headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some(EXT_ORIGIN)
        );
        assert!(
            resp.headers()
                .get("access-control-allow-credentials")
                .is_none(),
            "cookie credentials must stay unsupported for extension origins"
        );

        // The real request: extension origin + Bearer token → 200 with the
        // origin echoed (without it the browser discards the response).
        let resp = h
            .api(Method::GET, &alpha.subdomain, "/api/atoms")
            .header("Origin", EXT_ORIGIN)
            .bearer_auth(&alpha.token)
            .send()
            .await
            .expect("send get");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some(EXT_ORIGIN)
        );

        // A foreign web origin is outside the policy: no CORS headers.
        let resp = h
            .api(Method::OPTIONS, &alpha.subdomain, "/api/atoms")
            .header("Origin", "https://evil.example")
            .header("Access-Control-Request-Method", "GET")
            .send()
            .await
            .expect("send foreign preflight");
        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "foreign web origins must get no CORS allowance"
        );

        h.stop().await;
    })
    .await;
}

#[actix_web::test]
async fn unknown_subdomain_and_unauthenticated_requests() {
    with_control_db(
        "unknown_subdomain_and_unauthenticated_requests",
        |url| async move {
            let h = CloudHarness::spawn(&url, AccountCacheConfig::default()).await;
            let alpha = h.provision("alpha").await;

            // Unknown subdomain, even with a valid credential → 404.
            let resp = h
                .api(Method::GET, "ghost", "/api/atoms")
                .bearer_auth(&alpha.token)
                .send()
                .await
                .expect("send");
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);
            let body: Value = resp.json().await.expect("json");
            assert_eq!(body["error"], "not_found");

            // Valid subdomain, no credentials → 401.
            let resp = h
                .api(Method::GET, &alpha.subdomain, "/api/atoms")
                .send()
                .await
                .expect("send");
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

            // /health is public — no Host routing, no auth.
            let resp = h
                .client
                .get(format!("{}/health", h.base_url))
                .send()
                .await
                .expect("send");
            assert_eq!(resp.status(), StatusCode::OK);

            // The token subtree is cloud-owned now (tenant_plane serves it
            // against control-plane cloud_tokens; see
            // tenant_token_management_plane), but the REST of the
            // self-hosted auth plane stays unrouted even for an
            // authenticated tenant.
            let resp = h
                .api(Method::GET, &alpha.subdomain, "/api/auth/tokens")
                .bearer_auth(&alpha.token)
                .send()
                .await
                .expect("send");
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "the token subtree is cloud-owned and routed"
            );
            let resp = h
                .api(Method::GET, &alpha.subdomain, "/api/auth/setup")
                .bearer_auth(&alpha.token)
                .send()
                .await
                .expect("send");
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "the rest of the self-hosted auth plane must stay unrouted"
            );

            h.stop().await;
        },
    )
    .await;
}

/// The export-job and log planes are bound to composition-time state
/// (`AppState.export_jobs` / `AppState.log_buffer` — under cloud, the single
/// inert fallback shared by every tenant), making export jobs/artifacts and
/// the log ring buffer one process-global namespace: any authenticated
/// tenant could read or DELETE another tenant's export by id, or read
/// cross-tenant logs. The composition unroutes those planes; pin the
/// route-level 404 — including the guard's denial body, which distinguishes
/// "unrouted" from a handler's own not-found — for an authenticated tenant.
#[actix_web::test]
async fn export_and_log_planes_are_unrouted() {
    with_control_db("export_and_log_planes_are_unrouted", |url| async move {
        let h = CloudHarness::spawn(&url, AccountCacheConfig::default()).await;
        let alpha = h.provision("alpha").await;

        for (method, path) in [
            // Would otherwise start a job (202) against the fallback state.
            (Method::POST, "/api/databases/default/exports/markdown"),
            // Would otherwise read/delete jobs in the shared namespace.
            (Method::GET, "/api/exports/any-job-id"),
            (Method::DELETE, "/api/exports/any-job-id"),
            // Would otherwise return the process-wide log buffer (200).
            (Method::GET, "/api/logs"),
        ] {
            let resp = h
                .api(method.clone(), &alpha.subdomain, path)
                .bearer_auth(&alpha.token)
                .send()
                .await
                .expect("send");
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "{method} {path} must be unrouted in cloud"
            );
            let body: Value = resp.json().await.expect("denial json");
            assert_eq!(
                body["error"], "not_found",
                "{method} {path} must be denied by the route guard, not a handler"
            );
        }

        h.stop().await;
    })
    .await;
}

/// Session-cookie auth works for a normal API call on the account's own
/// subdomain.
#[actix_web::test]
async fn session_cookie_authenticates_api_calls() {
    with_control_db("session_cookie_authenticates_api_calls", |url| async move {
        let h = CloudHarness::spawn(&url, AccountCacheConfig::default()).await;
        let alpha = h.provision("alpha").await;

        let session = create_session(
            &h.control,
            &alpha.account_id,
            Duration::from_secs(3600),
            None,
            None,
        )
        .await
        .expect("create session");

        let atom = h.create_atom(&alpha, "Visible to the session too.").await;
        let atom_id = atom["id"].as_str().expect("atom id").to_string();

        let resp = h
            .api(Method::GET, &alpha.subdomain, "/api/atoms")
            .header("Cookie", format!("{SESSION_COOKIE}={session}"))
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), StatusCode::OK);
        let listing: Value = resp.json().await.expect("atoms json");
        assert!(
            atom_ids(&listing).contains(&atom_id.as_str()),
            "session-authenticated call must read the tenant's data"
        );

        h.stop().await;
    })
    .await;
}

/// WS isolation: a client on alpha's socket receives alpha's pipeline
/// events; an atom created on bravo produces no frame on alpha's socket.
#[actix_web::test]
async fn ws_events_are_tenant_isolated() {
    with_control_db("ws_events_are_tenant_isolated", |url| async move {
        let h = CloudHarness::spawn(&url, AccountCacheConfig::default()).await;
        let alpha = h.provision("alpha").await;
        let bravo = h.provision("bravo").await;

        let mut ws = h.ws_connect(&alpha).await;

        // Bravo's atom first, run to pipeline completion: any cross-tenant
        // leak would already be sitting in alpha's socket buffer.
        let bravo_atom = h.create_atom(&bravo, "Bravo's note about espresso.").await;
        let bravo_id = bravo_atom["id"].as_str().expect("atom id").to_string();
        h.poll_pipeline_done(&bravo, &bravo_id).await;

        let alpha_atom = h.create_atom(&alpha, "Alpha's note about pour-over.").await;
        let alpha_id = alpha_atom["id"].as_str().expect("atom id").to_string();

        let frames = collect_until(&mut ws, EVENT_DEADLINE, |e| {
            e["type"] == "EmbeddingComplete" && e["atom_id"] == alpha_id.as_str()
        })
        .await;

        assert!(
            !frames.is_empty(),
            "alpha's socket must stream alpha's events"
        );
        for frame in &frames {
            let text = serde_json::to_string(frame).expect("serialize frame");
            assert!(
                !text.contains(&bravo_id),
                "bravo's atom leaked onto alpha's socket: {text}"
            );
        }

        h.stop().await;
    })
    .await;
}

/// Eviction pinning (plan decision 2026-06-09): with alpha's WebSocket
/// connected, driving the cache past both eviction conditions (tiny TTL,
/// cap of one) skips alpha's entry, so a later atom's events still arrive
/// on the original socket — the channel was never orphaned.
#[actix_web::test]
async fn live_ws_pins_cache_entry_against_eviction() {
    with_control_db(
        "live_ws_pins_cache_entry_against_eviction",
        |url| async move {
            let h = CloudHarness::spawn(
                &url,
                AccountCacheConfig {
                    idle_ttl: Duration::from_millis(100),
                    max_entries: 1,
                    ..AccountCacheConfig::default()
                },
            )
            .await;
            let alpha = h.provision("alpha").await;
            let bravo = h.provision("bravo").await;

            // Alpha's WS subscribes to its cache entry's channel.
            let mut ws = h.ws_connect(&alpha).await;

            // Overflow the cap: bravo's load makes alpha the eviction candidate,
            // but the live receiver must pin it.
            h.list_atoms(&bravo).await;
            // Idle alpha past the TTL and sweep explicitly.
            tokio::time::sleep(Duration::from_millis(250)).await;
            h.cache.sweep().await;
            assert!(
                h.cache.contains(&alpha.account_id).await,
                "entry with a live WebSocket subscriber must survive eviction"
            );
            // One more insert pass over the still-over-cap cache.
            h.list_atoms(&bravo).await;

            // The pinned entry still owns the channel the socket subscribed to:
            // a new atom's events arrive on the original connection.
            let atom = h
                .create_atom(&alpha, "Created after eviction pressure.")
                .await;
            let atom_id = atom["id"].as_str().expect("atom id").to_string();
            collect_until(&mut ws, EVENT_DEADLINE, |e| {
                e["type"] == "EmbeddingComplete" && e["atom_id"] == atom_id.as_str()
            })
            .await;

            h.stop().await;
        },
    )
    .await;
}

/// The CLI deletion shape: a process-separate `delete_account` call that
/// never touches the serve process's cache. The serve process self-heals —
/// the deleted account's requests 404 at auth even while its stale cache
/// entry lingers (harmless; the idle TTL reclaims it), the tenant database
/// is gone, and other tenants are untouched. The HTTP route
/// (`http_account_deletion_end_to_end`) is the preferred path because it
/// *does* evict and sever; this pins the self-healing the CLI doc promises.
#[actix_web::test]
async fn cli_style_deletion_self_heals_without_eviction() {
    with_control_db(
        "cli_style_deletion_self_heals_without_eviction",
        |url| async move {
            let h = CloudHarness::spawn(&url, AccountCacheConfig::default()).await;
            let alpha = h.provision("alpha").await;
            let bravo = h.provision("bravo").await;

            let bravo_atom = h.create_atom(&bravo, "Bravo survives.").await;
            let bravo_id = bravo_atom["id"].as_str().expect("atom id").to_string();
            let alpha_atom = h.create_atom(&alpha, "Alpha is about to go.").await;
            let alpha_id = alpha_atom["id"].as_str().expect("atom id").to_string();
            h.poll_pipeline_done(&alpha, &alpha_id).await;

            // Library-level deletion, exactly as the CLI runs it: no cache
            // eviction. Alpha's entry is still cached afterwards.
            delete_account(
                &h.control,
                &h.cluster,
                &ManagedKeys::Disabled,
                // No billing provider in tests: the subscription-cancel step is
                // skipped (DEL-1 `billing` is `None`), exactly as the CLI/reaper paths.
                None,
                atomic_cloud::BackupPolicy::DisabledAcknowledged,
                atomic_cloud::DeleteLock::Acquire,
                &alpha.account_id,
                atomic_cloud::DEFAULT_BACKUP_TIMEOUT,
            )
            .await
            .expect("delete account");
            assert!(
                h.cache.contains(&alpha.account_id).await,
                "the CLI path leaves the serve cache entry in place"
            );

            // Alpha's subdomain no longer routes, stale cache entry or not.
            let resp = h
                .api(Method::GET, &alpha.subdomain, "/api/atoms")
                .bearer_auth(&alpha.token)
                .send()
                .await
                .expect("send");
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);

            // The tenant database is gone from the cluster.
            assert!(
                !database_exists(&alpha.db_name).await,
                "tenant database must be dropped"
            );

            // Bravo is untouched.
            let listing = h.list_atoms(&bravo).await;
            assert!(
                atom_ids(&listing).contains(&bravo_id.as_str()),
                "bravo must be unaffected by alpha's deletion"
            );

            h.stop().await;
        },
    )
    .await;
}

/// The authenticated deletion route, end to end (plan: "Provisioning
/// lifecycle" → "Account deletion"): a session-cookie DELETE with the
/// correct confirmation destroys the account — the tenant database is gone,
/// the subdomain 404s (including a repeat DELETE), the cache entry is
/// evicted *despite* a live WebSocket receiver (deletion bypasses the
/// eviction pinning of decision 2026-06-09), the severed socket closes
/// within a bounded wait, and bravo — mid-flight, with its own open socket —
/// is untouched.
#[actix_web::test]
async fn http_account_deletion_end_to_end() {
    with_control_db("http_account_deletion_end_to_end", |url| async move {
        let h = CloudHarness::spawn(&url, AccountCacheConfig::default()).await;
        let alpha = h.provision("alpha").await;
        let bravo = h.provision("bravo").await;

        // Bravo mid-flight: existing content and an open WebSocket, both of
        // which must ride through alpha's deletion untouched.
        let bravo_atom = h.create_atom(&bravo, "Bravo survives.").await;
        let bravo_id = bravo_atom["id"].as_str().expect("atom id").to_string();
        h.poll_pipeline_done(&bravo, &bravo_id).await;
        let mut bravo_ws = h.ws_connect(&bravo).await;

        // Alpha: content with a *finished* pipeline (so no background task
        // still holds a Sender clone and the post-deletion socket close is
        // deterministic) and a connected WebSocket — whose live receiver
        // pins alpha's cache entry against ordinary eviction, exactly the
        // rule deletion must cut through.
        let alpha_atom = h.create_atom(&alpha, "Alpha is about to go.").await;
        let alpha_id = alpha_atom["id"].as_str().expect("atom id").to_string();
        h.poll_pipeline_done(&alpha, &alpha_id).await;
        let mut alpha_ws = h.ws_connect(&alpha).await;
        assert!(h.cache.contains(&alpha.account_id).await);

        // Happy path: session cookie + correct confirmation.
        let session = create_session(
            &h.control,
            &alpha.account_id,
            Duration::from_secs(3600),
            None,
            None,
        )
        .await
        .expect("create session");
        let resp = h
            .api(Method::DELETE, &alpha.subdomain, "/api/account")
            .header("Cookie", format!("{SESSION_COOKIE}={session}"))
            .json(&json!({ "confirm": alpha.subdomain }))
            .send()
            .await
            .expect("send delete");
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = resp.json().await.expect("json");
        assert_eq!(body["status"], "deleted");
        assert_eq!(body["subdomain"], alpha.subdomain);

        // Evicted despite the live receiver: deletion is not idle-TTL
        // eviction, the pinning rule must not apply.
        assert!(
            !h.cache.contains(&alpha.account_id).await,
            "deletion must evict the cache entry even with a live WebSocket"
        );

        // The severed socket closes once the last Sender clone unwinds.
        await_ws_close(&mut alpha_ws, EVENT_DEADLINE).await;

        // The subdomain no longer routes — the account is gone, so
        // CloudAuth 404s everything, including a repeat DELETE.
        let resp = h
            .api(Method::GET, &alpha.subdomain, "/api/atoms")
            .bearer_auth(&alpha.token)
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let resp = h
            .api(Method::DELETE, &alpha.subdomain, "/api/account")
            .header("Cookie", format!("{SESSION_COOKIE}={session}"))
            .json(&json!({ "confirm": alpha.subdomain }))
            .send()
            .await
            .expect("send repeat delete");
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "repeat DELETE after deletion must 404 at auth"
        );

        // The tenant database is gone from the cluster.
        assert!(
            !database_exists(&alpha.db_name).await,
            "tenant database must be dropped"
        );

        // Bravo, mid-flight: listing intact, and its open socket still
        // streams a fresh atom's pipeline events — its channel was never
        // touched by alpha's eviction.
        let listing = h.list_atoms(&bravo).await;
        assert!(
            atom_ids(&listing).contains(&bravo_id.as_str()),
            "bravo must be unaffected by alpha's deletion"
        );
        let atom = h
            .create_atom(&bravo, "Created after alpha's deletion.")
            .await;
        let atom_id = atom["id"].as_str().expect("atom id").to_string();
        collect_until(&mut bravo_ws, EVENT_DEADLINE, |e| {
            e["type"] == "EmbeddingComplete" && e["atom_id"] == atom_id.as_str()
        })
        .await;

        h.stop().await;
    })
    .await;
}

/// Every refusal on the deletion route, each leaving the account fully
/// intact: database- and MCP-scoped tokens 403 (a KB-pinned integration
/// must not destroy the account), cross-tenant credentials 401, wrong or
/// missing confirmation 400, and the route doesn't exist on the app host.
/// Then the bearer-token happy path (the end-to-end test covers the session
/// path) proves the same account deletes cleanly once asked correctly.
#[actix_web::test]
async fn account_deletion_refusals() {
    with_control_db("account_deletion_refusals", |url| async move {
        let h = CloudHarness::spawn(&url, AccountCacheConfig::default()).await;
        let alpha = h.provision("alpha").await;
        let bravo = h.provision("bravo").await;

        // Database- and MCP-scoped tokens: correct confirmation, still 403.
        for (scope, name) in [
            (TokenScope::Database, "kb-pinned"),
            (TokenScope::Mcp, "mcp"),
        ] {
            let scoped = issue_token(&h.control, &alpha.account_id, scope, Some("default"), name)
                .await
                .expect("issue scoped token");
            let resp = h
                .api(Method::DELETE, &alpha.subdomain, "/api/account")
                .bearer_auth(&scoped)
                .json(&json!({ "confirm": alpha.subdomain }))
                .send()
                .await
                .expect("send scoped delete");
            assert_eq!(
                resp.status(),
                StatusCode::FORBIDDEN,
                "{name} token must not delete the account"
            );
            let body: Value = resp.json().await.expect("denial json");
            assert_eq!(body["error"], "account_scope_required");
        }

        // Bravo's account-scope token on alpha's subdomain: the cross-tenant
        // chokepoint refuses before the handler exists.
        let resp = h
            .api(Method::DELETE, &alpha.subdomain, "/api/account")
            .bearer_auth(&bravo.token)
            .json(&json!({ "confirm": alpha.subdomain }))
            .send()
            .await
            .expect("send cross-tenant delete");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Wrong confirmation → 400.
        let resp = h
            .api(Method::DELETE, &alpha.subdomain, "/api/account")
            .bearer_auth(&alpha.token)
            .json(&json!({ "confirm": "alpha-typo" }))
            .send()
            .await
            .expect("send mismatched delete");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body: Value = resp.json().await.expect("denial json");
        assert_eq!(body["error"], "confirmation_mismatch");

        // Missing body → the same structured 400; a stray DELETE can't fire.
        let resp = h
            .api(Method::DELETE, &alpha.subdomain, "/api/account")
            .bearer_auth(&alpha.token)
            .send()
            .await
            .expect("send bodyless delete");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body: Value = resp.json().await.expect("denial json");
        assert_eq!(body["error"], "confirmation_mismatch");

        // The route doesn't exist on the app plane: no subdomain label on
        // either app-host name → CloudAuth 404s before any handler.
        for host in [BASE_DOMAIN.to_string(), format!("app.{BASE_DOMAIN}")] {
            let resp = h
                .client
                .request(Method::DELETE, format!("{}/api/account", h.base_url))
                .header(HOST, host.clone())
                .bearer_auth(&alpha.token)
                .json(&json!({ "confirm": alpha.subdomain }))
                .send()
                .await
                .expect("send app-host delete");
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "DELETE /api/account must not exist on {host}"
            );
        }

        // Nothing above touched the account.
        let resp = h
            .api(Method::GET, &alpha.subdomain, "/api/atoms")
            .bearer_auth(&alpha.token)
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), StatusCode::OK, "account must be intact");
        assert!(
            database_exists(&alpha.db_name).await,
            "tenant database must still exist after refused deletions"
        );

        // Asked correctly — account-scope bearer token + matching
        // confirmation — the same account deletes.
        let resp = h
            .api(Method::DELETE, &alpha.subdomain, "/api/account")
            .bearer_auth(&alpha.token)
            .json(&json!({ "confirm": alpha.subdomain }))
            .send()
            .await
            .expect("send delete");
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = h
            .api(Method::GET, &alpha.subdomain, "/api/atoms")
            .bearer_auth(&alpha.token)
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        h.stop().await;
    })
    .await;
}

/// The fallback-unreachable guard (see `server.rs` module docs): a request
/// that somehow reaches the route table without the tenant extension fails
/// closed instead of being served from the inert fallback store. Composed
/// from the exact guard + scope + state the cloud composition uses, minus
/// `CloudAuth` (the only extension installer) — simulating the "somehow".
/// Needs no Postgres: the fallback is the SQLite scratch state.
#[actix_web::test]
async fn fallback_state_fails_closed_without_tenant_extension() {
    use actix_web::http::StatusCode;
    use actix_web::middleware::from_fn;
    use actix_web::test as actix_test;
    use atomic_server::app::api_scope;

    let fallback = FallbackAppState::build().expect("build fallback state");

    // Plant a canary directly in the fallback store. If any request below
    // were served from the fallback rather than failing closed, this is
    // what it would see.
    let core = fallback
        .data()
        .manager
        .active_core()
        .await
        .expect("fallback core");
    core.create_atom(
        atomic_core::CreateAtomRequest {
            content: "canary: this atom must be unreachable".to_string(),
            ..Default::default()
        },
        |_| {},
    )
    .await
    .expect("seed canary atom");

    let app = actix_test::init_service(
        actix_web::App::new()
            .app_data(fallback.data())
            .service(api_scope().wrap(from_fn(cloud_plane_guard))),
    )
    .await;

    // No CloudAuth ran, so no RequestDatabaseManager extension exists: the
    // guard must fail closed, not let the Db extractor fall back to the
    // canary's store.
    let resp = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri("/api/atoms")
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "extension-less request must fail closed"
    );
    let body: Value = actix_test::read_body_json(resp).await;
    assert_eq!(body["error"], "tenant_not_resolved");
    assert!(
        !serde_json::to_string(&body)
            .expect("json")
            .contains("canary"),
        "fallback data must never be served"
    );

    // Writes fail closed too.
    let resp = actix_test::call_service(
        &app,
        actix_test::TestRequest::post()
            .uri("/api/atoms")
            .set_json(json!({ "content": "must not land anywhere" }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

    // And the guard's other rule: the self-hosted auth plane — handlers
    // that bind the composition-time AppState manager (the fallback)
    // directly — is unrouted…
    let resp = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri("/api/auth/setup")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // …except the token subtree, which is cloud-owned (tenant_plane) and
    // carved out of the unrouting — so with no resolved tenant it
    // fail-closes through the guard's missing-extension rule instead.
    let resp = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri("/api/auth/tokens")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

/// The data plane writes dispatch hints (plan: "Worker fairness & job
/// queue" → "Cross-tenant ledger scan"): read-only requests leave the hint
/// table untouched, while a mutating request marks the authenticated
/// account's `dispatch_hints` row — durably, before the response returns,
/// so no polling is needed here. The hint-clearing semantics (including the
/// mid-scan bump survival bound) are pinned in `tests/dispatch_hints.rs`.
#[actix_web::test]
async fn mutating_requests_mark_dispatch_hints() {
    with_control_db("mutating_requests_mark_dispatch_hints", |url| async move {
        let h = CloudHarness::spawn(&url, AccountCacheConfig::default()).await;
        let alpha = h.provision("alpha").await;

        // Read-only traffic marks nothing.
        h.list_atoms(&alpha).await;
        assert!(
            list_hinted_accounts(&h.control)
                .await
                .expect("list hints")
                .is_empty(),
            "a GET must not mark a dispatch hint"
        );

        // A mutating request marks exactly this tenant's hint.
        let atom = h
            .create_atom(&alpha, "Alpha's note about Rust workspaces.")
            .await;
        let hinted: Vec<String> = list_hinted_accounts(&h.control)
            .await
            .expect("list hints")
            .into_iter()
            .map(|hint| hint.account_id)
            .collect();
        assert_eq!(
            hinted,
            vec![alpha.account_id.clone()],
            "a POST must mark the authenticated account's hint"
        );

        // Default mode keeps inline pipeline execution on — let it finish
        // before teardown drops the tenant database under it.
        let atom_id = atom["id"].as_str().expect("atom id").to_string();
        h.poll_pipeline_done(&alpha, &atom_id).await;
        h.stop().await;
    })
    .await;
}

/// The deploy-gating straggler gate, end to end (plan: "Schema migration on
/// deploy" → "Stragglers"): an account whose tenant database lags the
/// compiled schema target gets the structured 503 `account_upgrading` —
/// exact body, `Retry-After` header, on the data plane AND the WebSocket
/// upgrade — while accounts at the target, `/health`, and the account plane
/// are untouched; stamping the account current restores service. The lag is
/// driven by SQL (the honest simulation of a tenant the fleet runner hasn't
/// reached — fresh provisions are stamped current, as `tests/provisioning.rs`
/// pins).
#[actix_web::test]
async fn straggler_accounts_get_account_upgrading_503() {
    with_control_db(
        "straggler_accounts_get_account_upgrading_503",
        |url| async move {
            let h = CloudHarness::spawn(&url, AccountCacheConfig::default()).await;
            let alpha = h.provision("alpha").await;
            let bravo = h.provision("bravo").await;
            let target = tenant_schema_target();

            let stamp_alpha = |version: i32| {
                let control = h.control.clone();
                let account_id = alpha.account_id.clone();
                async move {
                    sqlx::query(
                        "UPDATE account_databases SET last_migrated_version = $2 \
                         WHERE account_id = $1",
                    )
                    .bind(&account_id)
                    .bind(version)
                    .execute(control.pool())
                    .await
                    .expect("stamp last_migrated_version");
                }
            };

            // Mark alpha as mid-upgrade.
            stamp_alpha(target - 1).await;

            // Data plane → the plan's 503, verbatim, with Retry-After.
            let resp = h
                .api(Method::GET, &alpha.subdomain, "/api/atoms")
                .bearer_auth(&alpha.token)
                .send()
                .await
                .expect("send");
            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(
                resp.headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .map(|v| v.as_bytes()),
                Some("60".as_bytes())
            );
            let body: Value = resp.json().await.expect("json");
            assert_eq!(
                body,
                json!({
                    "error": "account_upgrading",
                    "message": "Your account is being upgraded. Try again shortly.",
                    "retry_after_seconds": 60,
                }),
                "the straggler body is the plan's, verbatim"
            );

            // The gate fires before credentials are read (the
            // account_provisioning sibling's behavior).
            let resp = h
                .api(Method::GET, &alpha.subdomain, "/api/atoms")
                .send()
                .await
                .expect("send");
            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

            // The WebSocket upgrade is gated identically: 503, not a socket.
            let mut request = format!("ws://127.0.0.1:{}/ws", h.port)
                .into_client_request()
                .expect("ws request");
            request.headers_mut().insert(
                "Host",
                format!("alpha.{BASE_DOMAIN}").parse().expect("host header"),
            );
            request.headers_mut().insert(
                "Authorization",
                format!("Bearer {}", alpha.token)
                    .parse()
                    .expect("auth header"),
            );
            match tokio_tungstenite::connect_async(request).await {
                Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
                    assert_eq!(resp.status().as_u16(), 503, "ws upgrade must 503");
                    let body = resp.body().as_deref().unwrap_or_default();
                    assert!(
                        String::from_utf8_lossy(body).contains("account_upgrading"),
                        "ws denial carries the structured body"
                    );
                }
                other => panic!("ws connect must be refused with HTTP 503, got {other:?}"),
            }

            // An account at the target is untouched.
            h.list_atoms(&bravo).await;

            // /health never passes through the gate.
            let resp = h
                .client
                .get(format!("{}/health", h.base_url))
                .send()
                .await
                .expect("send");
            assert_eq!(resp.status(), StatusCode::OK);

            // The account plane (app host) never passes through it either:
            // a login link request for the upgrading account's email gets
            // the neutral 200, not a leaked 503.
            let resp = h
                .client
                .post(format!("{}/login/request-link", h.base_url))
                .header(HOST, BASE_DOMAIN)
                .json(&json!({ "email": "alpha@example.com" }))
                .send()
                .await
                .expect("send");
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "the account plane must not surface the straggler 503"
            );

            // Stamped current → serves again, including the WebSocket.
            stamp_alpha(target).await;
            h.list_atoms(&alpha).await;
            let ws = h.ws_connect(&alpha).await;
            drop(ws);

            h.stop().await;
        },
    )
    .await;
}

/// The account-plane SPA through the **composed** server, end to end:
///
/// - the app host serves the base-domain-injected shell on a client-routed
///   path;
/// - a tenant `/account/*` navigation is session-gated server-side — an
///   unauthenticated one is a `302` to the app-host login (never a flash of
///   the dashboard shell), a session-cookie'd one serves the shell;
/// - and — critically — the JSON planes (`/health`, the unauthenticated
///   tenant `/api/*`) still resolve and are never shadowed by the SPA: an
///   unauthenticated API call is the structured JSON `401`, not the redirect.
#[actix_web::test]
async fn spa_serves_app_gates_tenant_dashboard_and_never_shadows_json() {
    with_control_db(
        "spa_serves_app_gates_tenant_dashboard_and_never_shadows_json",
        |url| async move {
            let h = CloudHarness::spawn(&url, AccountCacheConfig::default()).await;
            let tenant = h.provision("alpha").await;

            // A client that does NOT follow redirects, so the `302` from the
            // account gate is observable as a `302` rather than transparently
            // followed to the login page.
            let no_redirect = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("build non-redirecting client");

            // App host (`app.<base>` and the bare base): a client-routed path
            // with no file → the SPA shell, base domain injected into the meta.
            for host in [format!("app.{BASE_DOMAIN}"), BASE_DOMAIN.to_string()] {
                for path in ["/", "/login", "/signup"] {
                    let resp = h
                        .client
                        .get(format!("{}{path}", h.base_url))
                        .header(HOST, &host)
                        .send()
                        .await
                        .expect("send app-host page");
                    assert_eq!(resp.status(), StatusCode::OK, "{host}{path}");
                    let ct = resp
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    assert!(ct.contains("text/html"), "{host}{path} is HTML: {ct}");
                    let body = resp.text().await.expect("body");
                    assert!(
                        body.contains(&format!(r#"content="{BASE_DOMAIN}""#)),
                        "{host}{path} carries the injected base domain"
                    );
                }
            }

            // Tenant subdomain, NO session: an `/account/*` navigation is a
            // `302` to the app-host login — the dashboard chrome is never sent
            // to an unauthenticated browser.
            let resp = no_redirect
                .get(format!("{}/account/provider", h.base_url))
                .header(HOST, format!("{}.{BASE_DOMAIN}", tenant.subdomain))
                .send()
                .await
                .expect("send unauth tenant deep link");
            assert_eq!(
                resp.status(),
                StatusCode::FOUND,
                "unauthenticated /account/* redirects, not 200 shell"
            );
            let location = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            assert!(
                location.ends_with(&format!("app.{BASE_DOMAIN}/login")),
                "redirect targets the app-host login: {location}"
            );

            // Tenant subdomain, WITH a valid session: the same navigation now
            // serves the SPA shell (HTML 200, base domain injected).
            let session = create_session(
                &h.control,
                &tenant.account_id,
                Duration::from_secs(3600),
                None,
                None,
            )
            .await
            .expect("create session");
            let resp = no_redirect
                .get(format!("{}/account/provider", h.base_url))
                .header(HOST, format!("{}.{BASE_DOMAIN}", tenant.subdomain))
                .header("Cookie", format!("{SESSION_COOKIE}={session}"))
                .send()
                .await
                .expect("send authed tenant deep link");
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "a valid session serves the dashboard shell"
            );
            let ct = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            assert!(ct.contains("text/html"), "authed deep link is HTML: {ct}");
            let body = resp.text().await.expect("body");
            assert!(
                body.contains(&format!(r#"content="{BASE_DOMAIN}""#)),
                "authed dashboard shell carries the injected base domain"
            );

            // A session for ANOTHER tenant must NOT unlock this dashboard (the
            // cookie crosses subdomains by design — the account-scoped verify
            // is the chokepoint). Presented on bravo's subdomain, alpha's
            // session redirects exactly like no cookie at all.
            let bravo = h.provision("bravo").await;
            let resp = no_redirect
                .get(format!("{}/account/billing", h.base_url))
                .header(HOST, format!("{}.{BASE_DOMAIN}", bravo.subdomain))
                .header("Cookie", format!("{SESSION_COOKIE}={session}"))
                .send()
                .await
                .expect("send cross-tenant session");
            assert_eq!(
                resp.status(),
                StatusCode::FOUND,
                "alpha's session does not unlock bravo's dashboard"
            );

            // JSON planes are NOT shadowed:
            // - `/health` resolves to its JSON on any host.
            let resp = h
                .client
                .get(format!("{}/health", h.base_url))
                .header(HOST, format!("app.{BASE_DOMAIN}"))
                .send()
                .await
                .expect("send health");
            assert_eq!(resp.status(), StatusCode::OK);
            let ct = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            assert!(ct.contains("application/json"), "health stays JSON: {ct}");

            // - the tenant `/api/account/overview` without auth returns the
            //   API's structured 401 (CloudAuth), NOT the gate's HTML redirect:
            //   an API call (or the dashboard's background fetch) gets JSON.
            let resp = no_redirect
                .get(format!("{}/api/account/overview", h.base_url))
                .header(HOST, format!("{}.{BASE_DOMAIN}", tenant.subdomain))
                .send()
                .await
                .expect("send unauth overview");
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "unauthenticated overview is a JSON 401, not the gate redirect"
            );
            let ct = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            assert!(
                ct.contains("application/json"),
                "unauth overview stays JSON (not shadowed): {ct}"
            );

            h.stop().await;
        },
    )
    .await;
}

// ==================== Migration imports ====================

/// POST an upload to `/api/migrations/sqlite` for `tenant` under `token`.
async fn send_import(
    h: &CloudHarness,
    subdomain: &str,
    token: &str,
    name: &str,
    body: Vec<u8>,
) -> reqwest::Response {
    h.api(Method::POST, subdomain, "/api/migrations/sqlite")
        .query(&[("name", name)])
        .bearer_auth(token)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(body)
        .send()
        .await
        .expect("send migration upload")
}

/// Poll a migration job until it reaches a terminal state.
async fn poll_migration_job(h: &CloudHarness, tenant: &Tenant, job_id: &str) -> Value {
    let deadline = tokio::time::Instant::now() + EVENT_DEADLINE;
    loop {
        let resp = h
            .api(
                Method::GET,
                &tenant.subdomain,
                &format!("/api/migrations/{job_id}"),
            )
            .bearer_auth(&tenant.token)
            .send()
            .await
            .expect("poll migration job");
        assert_eq!(resp.status(), StatusCode::OK, "poll migration job");
        let job: Value = resp.json().await.expect("migration job json");
        match job["status"].as_str() {
            Some("complete") | Some("failed") | Some("cancelled") => return job,
            _ if tokio::time::Instant::now() > deadline => {
                panic!("migration job never reached a terminal state: {job}")
            }
            _ => tokio::time::sleep(Duration::from_millis(250)).await,
        }
    }
}

/// The full tenant-aware import path: an account-scoped upload lands as a new
/// knowledge base in the uploader's tenant database (and nowhere else), the
/// job is invisible to other accounts, a database-pinned token may not
/// import, and re-importing the same source fails on the PK collision.
#[actix_web::test]
async fn tenant_migration_import_end_to_end() {
    with_control_db("migration_import_e2e", |url| async move {
        // `QuotaBilling::for_tests` (inside the harness) widens every plan
        // to unlimited, so this test exercises the transport; the plan
        // ceilings are pinned by tests/quota_billing.rs.
        let h = CloudHarness::spawn(&url, AccountCacheConfig::default()).await;
        let alpha = h.provision("alpha").await;
        let bravo = h.provision("bravo").await;
        let snapshot =
            support::sqlite_snapshot_fixture(&["First note about ownership.", "Second note."])
                .await;

        // A database-pinned token mints no new KBs.
        let pinned = issue_token(
            &h.control,
            &alpha.account_id,
            TokenScope::Database,
            Some("default"),
            "pinned",
        )
        .await
        .expect("issue db-pinned token");
        let resp = send_import(
            &h,
            &alpha.subdomain,
            &pinned,
            "Imported KB",
            snapshot.clone(),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "db-scoped tokens cannot import"
        );

        // Account-scoped upload → background import job → complete.
        let resp = send_import(
            &h,
            &alpha.subdomain,
            &alpha.token,
            "Imported KB",
            snapshot.clone(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED, "upload accepted");
        let job: Value = resp.json().await.expect("job json");
        let job_id = job["id"].as_str().expect("job id").to_string();

        // Another account cannot even confirm the job exists.
        let foreign = h
            .api(
                Method::GET,
                &bravo.subdomain,
                &format!("/api/migrations/{job_id}"),
            )
            .bearer_auth(&bravo.token)
            .send()
            .await
            .expect("foreign job poll");
        assert_eq!(
            foreign.status(),
            StatusCode::NOT_FOUND,
            "jobs are tenant-scoped"
        );

        let done = poll_migration_job(&h, &alpha, &job_id).await;
        assert_eq!(done["status"], "complete", "import completes: {done}");
        let new_db = done["db_id"].as_str().expect("imported db id").to_string();

        // The KB exists for alpha with the imported atoms — and not for bravo.
        let dbs: Value = h
            .api(Method::GET, &alpha.subdomain, "/api/databases")
            .bearer_auth(&alpha.token)
            .send()
            .await
            .expect("list alpha dbs")
            .json()
            .await
            .expect("alpha dbs json");
        assert!(
            dbs["databases"]
                .as_array()
                .expect("databases array")
                .iter()
                .any(|d| d["name"] == "Imported KB"),
            "imported KB appears in alpha's catalog: {dbs}"
        );
        let bravo_dbs: Value = h
            .api(Method::GET, &bravo.subdomain, "/api/databases")
            .bearer_auth(&bravo.token)
            .send()
            .await
            .expect("list bravo dbs")
            .json()
            .await
            .expect("bravo dbs json");
        assert!(
            !bravo_dbs["databases"]
                .as_array()
                .expect("databases array")
                .iter()
                .any(|d| d["name"] == "Imported KB"),
            "imported KB never leaks into bravo's catalog"
        );

        let atoms: Value = h
            .api(Method::GET, &alpha.subdomain, "/api/atoms")
            .header("X-Atomic-Database", new_db.as_str())
            .bearer_auth(&alpha.token)
            .send()
            .await
            .expect("list imported atoms")
            .json()
            .await
            .expect("imported atoms json");
        assert_eq!(atoms["total_count"], 2, "imported atoms visible: {atoms}");

        // Re-importing the same source aborts on the PK collision.
        let resp = send_import(
            &h,
            &alpha.subdomain,
            &alpha.token,
            "Duplicate",
            snapshot.clone(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let dup: Value = resp.json().await.expect("dup job json");
        let dup_id = dup["id"].as_str().expect("dup job id").to_string();
        let failed = poll_migration_job(&h, &alpha, &dup_id).await;
        assert_eq!(failed["status"], "failed", "duplicate import fails");
        assert!(
            failed["error"]
                .as_str()
                .unwrap_or_default()
                .contains("already exist"),
            "collision is reported: {failed}"
        );

        h.stop().await;
    })
    .await;
}

/// The tenant token-management plane (`/api/auth/tokens*`): mint → list →
/// use → revoke → dead, with the scope-escalation and cross-tenant
/// chokepoints asserted along the way. This is the surface the product
/// app's settings UI drives, and the token source for the desktop
/// "Migrate to Cloud" flow.
#[actix_web::test]
async fn tenant_token_management_plane() {
    with_control_db("tenant_token_management_plane", |url| async move {
        let h = CloudHarness::spawn(&url, AccountCacheConfig::default()).await;
        let alpha = h.provision("alpha").await;
        let bravo = h.provision("bravo").await;

        // Mint through the route; the plaintext appears exactly once.
        let resp = h
            .api(Method::POST, &alpha.subdomain, "/api/auth/tokens")
            .bearer_auth(&alpha.token)
            .json(&json!({ "name": "migration-laptop" }))
            .send()
            .await
            .expect("create token");
        assert_eq!(resp.status(), StatusCode::CREATED);
        let created: Value = resp.json().await.expect("created json");
        let minted = created["token"].as_str().expect("plaintext").to_string();
        let minted_id = created["id"].as_str().expect("id").to_string();
        assert!(minted.starts_with("atm_"), "cloud token prefix");
        assert_eq!(
            created["prefix"].as_str().expect("prefix"),
            &minted[..10],
            "display prefix is the plaintext's first 10 chars"
        );

        // It lists — metadata only, prefix present, never the plaintext.
        let resp = h
            .api(Method::GET, &alpha.subdomain, "/api/auth/tokens")
            .bearer_auth(&alpha.token)
            .send()
            .await
            .expect("list tokens");
        assert_eq!(resp.status(), StatusCode::OK);
        let listing: Value = resp.json().await.expect("list json");
        let entries = listing.as_array().expect("array");
        assert!(
            entries.iter().any(|t| t["name"] == "migration-laptop"
                && t["token_prefix"] == created["prefix"]
                && t["is_revoked"] == false),
            "minted token appears with its metadata: {listing}"
        );
        assert!(
            !listing.to_string().contains(&minted),
            "the plaintext must never appear in a listing"
        );

        // The minted token is a working data-plane credential.
        let resp = h
            .api(Method::GET, &alpha.subdomain, "/api/atoms")
            .bearer_auth(&minted)
            .send()
            .await
            .expect("use minted token");
        assert_eq!(resp.status(), StatusCode::OK);

        // Escalation chokepoint: a db-pinned credential may not mint an
        // account-scope token (nor list or revoke).
        let pinned = issue_token(
            &h.control,
            &alpha.account_id,
            TokenScope::Database,
            Some("default"),
            "kb-pinned",
        )
        .await
        .expect("issue pinned token");
        for (method, path) in [
            (Method::POST, "/api/auth/tokens".to_string()),
            (Method::GET, "/api/auth/tokens".to_string()),
            (Method::DELETE, format!("/api/auth/tokens/{minted_id}")),
        ] {
            let mut req = h.api(method.clone(), &alpha.subdomain, &path).bearer_auth(&pinned);
            if method == Method::POST {
                req = req.json(&json!({ "name": "escalation" }));
            }
            let resp = req.send().await.expect("scoped request");
            assert_eq!(
                resp.status(),
                StatusCode::FORBIDDEN,
                "{method} {path} must 403 for a db-pinned credential"
            );
        }

        // Cross-tenant chokepoint: bravo can neither see nor revoke alpha's
        // token — the foreign id reads as not-found.
        let resp = h
            .api(
                Method::DELETE,
                &bravo.subdomain,
                &format!("/api/auth/tokens/{minted_id}"),
            )
            .bearer_auth(&bravo.token)
            .send()
            .await
            .expect("cross-tenant revoke");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let resp = h
            .api(Method::GET, &bravo.subdomain, "/api/auth/tokens")
            .bearer_auth(&bravo.token)
            .send()
            .await
            .expect("bravo list");
        let bravo_listing: Value = resp.json().await.expect("bravo json");
        assert!(
            bravo_listing
                .as_array()
                .expect("array")
                .iter()
                .all(|t| t["name"] != "migration-laptop"),
            "alpha's token must never appear in bravo's listing"
        );

        // Revoke for real: the route confirms, the credential dies, and the
        // listing no longer shows it.
        let resp = h
            .api(
                Method::DELETE,
                &alpha.subdomain,
                &format!("/api/auth/tokens/{minted_id}"),
            )
            .bearer_auth(&alpha.token)
            .send()
            .await
            .expect("revoke");
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = h
            .api(Method::GET, &alpha.subdomain, "/api/atoms")
            .bearer_auth(&minted)
            .send()
            .await
            .expect("use revoked token");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let resp = h
            .api(Method::GET, &alpha.subdomain, "/api/auth/tokens")
            .bearer_auth(&alpha.token)
            .send()
            .await
            .expect("list after revoke");
        let after: Value = resp.json().await.expect("json");
        assert!(
            after
                .as_array()
                .expect("array")
                .iter()
                .all(|t| t["id"] != minted_id.as_str()),
            "revoked token must leave the listing"
        );

        h.stop().await;
    })
    .await;
}

/// The admin plane: existence-hidden to everyone but an is_admin session,
/// and the plan-override path end to end — catalogue-validated, ledgered
/// as trigger='admin', audited, and pinned against the trial sweep.
#[actix_web::test]
async fn admin_plane_gates_and_plan_override() {
    with_control_db("admin_plane_gates_and_plan_override", |url| async move {
        let h = CloudHarness::spawn(&url, AccountCacheConfig::default()).await;
        let alpha = h.provision("alpha").await;
        let bravo = h.provision("bravo").await;

        let app_host = format!("app.{BASE_DOMAIN}");
        let admin_get = |path: &str, cookie: Option<String>| {
            let mut req = h
                .client
                .get(format!("{}{path}", h.base_url))
                .header(HOST, app_host.as_str());
            if let Some(cookie) = cookie {
                req = req.header("Cookie", format!("{SESSION_COOKIE}={cookie}"));
            }
            req
        };

        // No cookie, a bogus cookie, and a real-but-not-admin session all
        // read identically: the plane does not exist for you.
        let alpha_session = create_session(
            &h.control,
            &alpha.account_id,
            Duration::from_secs(3600),
            None,
            None,
        )
        .await
        .expect("alpha session");
        for cookie in [
            None,
            Some("ats_bogus".to_string()),
            Some(alpha_session.clone()),
        ] {
            let resp = admin_get("/admin/api/accounts", cookie)
                .send()
                .await
                .expect("send");
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "non-admin must read the admin plane as not-found"
            );
        }

        // Promote alpha (the CLI path) — the same session now reads the plane.
        atomic_cloud::admin::set_admin_flag(&h.control, "alpha", true)
            .await
            .expect("promote alpha");
        let resp = admin_get("/admin/api/accounts", Some(alpha_session.clone()))
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), StatusCode::OK);
        let listing: Value = resp.json().await.expect("listing json");
        assert!(
            listing
                .as_array()
                .expect("array")
                .iter()
                .any(|a| a["subdomain"] == "bravo"),
            "admin listing shows every account: {listing}"
        );

        // The plan picker's source of truth: the catalogue, comp included.
        let resp = admin_get("/admin/api/plans", Some(alpha_session.clone()))
            .send()
            .await
            .expect("send");
        let plans: Value = resp.json().await.expect("plans json");
        assert!(
            plans
                .as_array()
                .expect("array")
                .iter()
                .any(|p| p["id"] == "comp"),
            "comp tier seeded and listed: {plans}"
        );

        // Comp bravo through the route (defaults to pinned).
        let resp = h
            .client
            .put(format!(
                "{}/admin/api/accounts/{}/plan",
                h.base_url, bravo.account_id
            ))
            .header(HOST, app_host.as_str())
            .header("Cookie", format!("{SESSION_COOKIE}={alpha_session}"))
            .json(&json!({ "plan_id": "comp" }))
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), StatusCode::OK);

        let mut conn = PgConnection::connect(&url).await.expect("connect");
        let (plan_id, pinned): (Option<String>, bool) = sqlx::query_as(
            "SELECT plan_id, plan_pinned FROM accounts WHERE id = $1",
        )
        .bind(&bravo.account_id)
        .fetch_one(&mut conn)
        .await
        .expect("read account");
        assert_eq!(plan_id.as_deref(), Some("comp"));
        assert!(pinned, "admin plan override pins by default");
        let ledgered: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM plan_transitions \
             WHERE account_id = $1 AND trigger = 'admin' AND to_plan_id = 'comp'",
        )
        .bind(&bravo.account_id)
        .fetch_one(&mut conn)
        .await
        .expect("count transitions");
        assert_eq!(ledgered, 1, "override rides the plan_transitions ledger");
        let audited: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM admin_actions \
             WHERE action = 'set_plan' AND target_account_id = $1 AND actor = $2",
        )
        .bind(&bravo.account_id)
        .bind(&alpha.account_id)
        .fetch_one(&mut conn)
        .await
        .expect("count audits");
        assert_eq!(audited, 1, "override writes the admin audit ledger");

        // The pin holds against the trial sweep: manufacture an expired
        // trial on the comped account and run the sweep — the plan stays.
        sqlx::query(
            "UPDATE accounts SET billing_state = 'trialing', \
             trial_ends_at = NOW() - interval '1 day' WHERE id = $1",
        )
        .bind(&bravo.account_id)
        .execute(&mut conn)
        .await
        .expect("manufacture expired trial");
        atomic_cloud::billing::dunning::advance_expired_trials(
            &h.control,
            &ManagedKeys::Disabled,
            chrono::Utc::now(),
            |_| async { Ok(false) },
        )
        .await
        .expect("sweep");
        let plan_after: Option<String> =
            sqlx::query_scalar("SELECT plan_id FROM accounts WHERE id = $1")
                .bind(&bravo.account_id)
                .fetch_one(&mut conn)
                .await
                .expect("read plan after sweep");
        assert_eq!(
            plan_after.as_deref(),
            Some("comp"),
            "a pinned plan holds against the trial-expiry sweep"
        );

        // An unknown plan id is a structured 400, and nothing changes.
        let resp = h
            .client
            .put(format!(
                "{}/admin/api/accounts/{}/plan",
                h.base_url, bravo.account_id
            ))
            .header(HOST, app_host.as_str())
            .header("Cookie", format!("{SESSION_COOKIE}={alpha_session}"))
            .json(&json!({ "plan_id": "platinum-sparkle" }))
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // The admin plane never exists on a tenant subdomain, even for the
        // admin's own session: the host guard excludes it from the route
        // table, so the path falls through to the SPA fallback like any
        // unknown tenant path — an HTML shell, never JSON, never data.
        let resp = h
            .api(Method::GET, &alpha.subdomain, "/admin/api/accounts")
            .header("Cookie", format!("{SESSION_COOKIE}={alpha_session}"))
            .send()
            .await
            .expect("send");
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            content_type.contains("text/html"),
            "tenant-host /admin/api/* is the SPA shell, not the admin plane: {content_type}"
        );
        let body = resp.text().await.expect("body");
        assert!(
            !body.contains(&bravo.account_id) && !body.contains("bravo"),
            "no admin data on a tenant host"
        );

        h.stop().await;
    })
    .await;
}
