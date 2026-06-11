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
    provision_account, AccountCache, AccountCacheConfig, AccountPlane, AccountPlaneConfig,
    CloudAuth, ClusterConfig, ControlPlane, FallbackAppState, NewAccount, TokenScope,
    SESSION_COOKIE,
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
}

impl CloudHarness {
    /// Spawn the composition exactly as `atomic-cloud serve` wires it:
    /// migrated control plane, `AccountCache` (with the given config so the
    /// eviction test can shrink it), `CloudAuth`, fallback state, one worker
    /// on `127.0.0.1:0`.
    async fn spawn(control_url: &str, cache_config: AccountCacheConfig) -> Self {
        let control = ControlPlane::connect(control_url)
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
            cache_config,
        ));
        let auth = CloudAuth::new(control.clone(), Arc::clone(&cache), BASE_DOMAIN);
        // This suite never drives the account plane (tests/account_plane.rs
        // owns that); a capturing sender keeps any accidental traffic from
        // sending mail.
        let account_plane = AccountPlane::new(
            control.clone(),
            Arc::new(support::CapturingSender::default()),
            AccountPlaneConfig::new(BASE_DOMAIN),
        );
        let fallback = FallbackAppState::build().expect("build fallback state");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local addr").port();
        let state = fallback.data();
        let server = HttpServer::new(move || {
            App::new().configure(configure_cloud_app(
                state.clone(),
                auth.clone(),
                account_plane.clone(),
            ))
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
        }
    }

    async fn stop(self) {
        self.handle.stop(false).await;
    }

    /// Provision an account, seed its provider settings at the mock AI
    /// server (mirroring atomic-server's `TestCtx`), and issue an
    /// account-scope token.
    async fn provision(&self, subdomain: &str) -> Tenant {
        let account = provision_account(
            &self.control,
            &self.cluster,
            NewAccount {
                email: format!("{subdomain}@example.com"),
                subdomain: subdomain.to_string(),
            },
        )
        .await
        .expect("provision account");

        let tenant_url = self
            .cluster
            .tenant_db_url(&account.db_name)
            .expect("tenant url");
        let manager = DatabaseManager::new_postgres(".", &tenant_url)
            .await
            .expect("open tenant manager");
        let core = manager.active_core().await.expect("active core");
        let mock_url = self.mock.base_url();
        for (k, v) in [
            ("provider", "openai_compat"),
            ("openai_compat_base_url", mock_url.as_str()),
            ("openai_compat_api_key", "test-key"),
            ("openai_compat_embedding_model", "mock-embed"),
            ("openai_compat_llm_model", "mock-llm"),
            ("openai_compat_embedding_dimension", "1536"),
            ("auto_tagging_enabled", "true"),
        ] {
            core.set_setting(k, v).await.expect("seed tenant setting");
        }
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

            // The self-hosted token plane is unrouted in cloud, even for an
            // authenticated tenant: cloud tokens live in the control plane.
            let resp = h
                .api(Method::GET, &alpha.subdomain, "/api/auth/tokens")
                .bearer_auth(&alpha.token)
                .send()
                .await
                .expect("send");
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "self-hosted token plane must be unrouted"
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

/// Deletion: after deleting alpha, requests to its subdomain 404, its
/// tenant database is gone from `pg_database`, and bravo is unaffected.
#[actix_web::test]
async fn account_deletion_end_to_end() {
    with_control_db("account_deletion_end_to_end", |url| async move {
        let h = CloudHarness::spawn(&url, AccountCacheConfig::default()).await;
        let alpha = h.provision("alpha").await;
        let bravo = h.provision("bravo").await;

        let bravo_atom = h.create_atom(&bravo, "Bravo survives.").await;
        let bravo_id = bravo_atom["id"].as_str().expect("atom id").to_string();
        h.create_atom(&alpha, "Alpha is about to go.").await;

        // The deletion sequence: evict the serving resources, then delete.
        h.cache.evict(&alpha.account_id).await;
        delete_account(&h.control, &h.cluster, &alpha.account_id)
            .await
            .expect("delete account");

        // Alpha's subdomain no longer routes.
        let resp = h
            .api(Method::GET, &alpha.subdomain, "/api/atoms")
            .bearer_auth(&alpha.token)
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // The tenant database is gone from the cluster.
        let base_url = std::env::var("ATOMIC_TEST_DATABASE_URL").expect("env");
        let mut conn = PgConnection::connect(&base_url).await.expect("connect");
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)")
                .bind(&alpha.db_name)
                .fetch_one(&mut conn)
                .await
                .expect("query pg_database");
        let _ = conn.close().await;
        assert!(!exists, "tenant database must be dropped");

        // Bravo is untouched.
        let listing = h.list_atoms(&bravo).await;
        assert!(
            atom_ids(&listing).contains(&bravo_id.as_str()),
            "bravo must be unaffected by alpha's deletion"
        );

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

    // And the guard's other rule: the self-hosted token plane — the one
    // /api family whose handlers bind the composition-time AppState manager
    // (the fallback) directly — is unrouted.
    let resp = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri("/api/auth/tokens")
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
