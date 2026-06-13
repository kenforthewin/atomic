//! End-to-end tests for the provider plumbing and settings API (plan:
//! "Provider management" — "Plumbing", "BYOK entry & validation", "Live
//! rotation", "Model curation", "Audit / visibility").
//!
//! Each test spawns the real composition — `configure_cloud_app` on an
//! ephemeral port, exactly as `atomic-cloud serve` wires it — with managed
//! provisioning backed by a [`RecordingProvisioning`] and every AI call
//! pointed at `MockAiServer`s (NO REAL PROVIDERS, NO REAL EMAIL, EVER).
//! Key-shaped assertions are central here: response bodies are collected
//! and scanned for every secret in play (managed key plaintext, BYOK key
//! plaintext, the master key in hex and base64), and the control database
//! is scanned column-by-column for plaintext at rest.
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use actix_web::{App, HttpServer};
use atomic_cloud::{
    configure_cloud_app, issue_token, provision_account, AccountCache, AccountCacheConfig,
    AccountPlane, AccountPlaneConfig, ChatStreamLimiter, CloudAuth, ClusterConfig, ControlPlane,
    FallbackAppState, ManagedKeyConfig, ManagedKeys, NewAccount, ProvisionedAccount, QuotaBilling,
    Readiness, TenantPlane, TokenScope, DEFAULT_CHAT_STREAMS_PER_ACCOUNT, MANAGED_LLM_MODELS,
    SESSION_COOKIE,
};
use atomic_core::DatabaseManager;
use atomic_test_support::MockAiServer;
use reqwest::header::{HOST, LOCATION, SET_COOKIE};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use support::{
    control_db_contains, managed_keys_with_config, with_control_db, CapturingSender,
    RecordingProvisioning, TEST_MASTER_KEY,
};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Base domain the composition is configured with; accounts are addressed
/// as `<subdomain>.cloudtest.local` while TCP goes to `127.0.0.1`.
const BASE_DOMAIN: &str = "cloudtest.local";

/// How long to wait for a pipeline to reach a terminal state.
const PIPELINE_DEADLINE: Duration = Duration::from_secs(15);

/// The managed `model_config` seeded at signup in these tests: the curated
/// defaults plus a base-URL override pointing OpenRouter traffic at the
/// managed `MockAiServer`. The override is platform-side configuration
/// (composition-time), exactly the knob a proxy deployment would use — user
/// writes can never set it (curation rejects base-URL keys on managed rows).
fn managed_model_config(managed_mock: &MockAiServer) -> Value {
    json!({
        "embedding_model": "openai/text-embedding-3-small",
        "llm_model": "openai/gpt-4o-mini",
        "openrouter_base_url": managed_mock.base_url(),
    })
}

/// The composed cloud server with managed-key provisioning over a recording
/// API, plus the managed `MockAiServer` its model config points at.
struct ProviderHarness {
    control: ControlPlane,
    cluster: ClusterConfig,
    cache: Arc<AccountCache>,
    managed: ManagedKeys,
    /// The recording provisioning API behind `managed`.
    api: Arc<RecordingProvisioning>,
    /// Where managed-config (OpenRouter) AI traffic lands.
    managed_mock: MockAiServer,
    sender: CapturingSender,
    client: reqwest::Client,
    base_url: String,
    handle: actix_web::dev::ServerHandle,
    /// Every response body this harness has read, for the key-material scan.
    bodies: Mutex<Vec<String>>,
    _fallback: FallbackAppState,
}

impl ProviderHarness {
    /// Spawn with managed provisioning enabled (the recording API + the
    /// managed mock's base-URL override).
    async fn spawn_managed(control_url: &str) -> Self {
        let managed_mock = MockAiServer::start().await;
        let api = Arc::new(RecordingProvisioning::default());
        let managed = managed_keys_with_config(
            Arc::clone(&api),
            ManagedKeyConfig {
                model_config: managed_model_config(&managed_mock),
                ..ManagedKeyConfig::default()
            },
        );
        Self::spawn(control_url, managed, api, managed_mock).await
    }

    /// Spawn with managed provisioning disabled — accounts start keyless.
    async fn spawn_disabled(control_url: &str) -> Self {
        Self::spawn(
            control_url,
            ManagedKeys::Disabled,
            Arc::new(RecordingProvisioning::default()),
            MockAiServer::start().await,
        )
        .await
    }

    async fn spawn(
        control_url: &str,
        managed: ManagedKeys,
        api: Arc<RecordingProvisioning>,
        managed_mock: MockAiServer,
    ) -> Self {
        let control = ControlPlane::connect(control_url)
            .await
            .expect("connect control plane");
        control.initialize().await.expect("migrate control plane");
        let cluster = ClusterConfig {
            cluster_id: "test-cluster-1".to_string(),
            cluster_url: std::env::var("ATOMIC_TEST_DATABASE_URL")
                .expect("with_control_db verified ATOMIC_TEST_DATABASE_URL"),
        };
        let cache = Arc::new(AccountCache::new(
            control.clone(),
            cluster.clone(),
            support::test_vault(),
            AccountCacheConfig::default(),
        ));
        let auth = CloudAuth::new(control.clone(), Arc::clone(&cache), BASE_DOMAIN);
        let sender = CapturingSender::default();
        let account_plane = AccountPlane::new(
            control.clone(),
            cluster.clone(),
            managed.clone(),
            Arc::new(sender.clone()),
            AccountPlaneConfig::new(BASE_DOMAIN),
        )
        .expect("build account plane");
        let tenant_plane = TenantPlane::new(
            control.clone(),
            cluster.clone(),
            managed.clone(),
            support::test_vault(),
            Arc::clone(&cache),
        );
        let fallback = FallbackAppState::build().expect("build fallback state");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local addr").port();
        let state = fallback.data();
        let oauth_plane = atomic_cloud::OAuthPlane::new(
            control.clone(),
            BASE_DOMAIN,
            "http",
            format!("http://app.{BASE_DOMAIN}"),
        );
        let mcp_transport = fallback.mcp_transport(atomic_cloud::DEFAULT_MCP_SSE_KEEP_ALIVE);
        let control_for_app = control.clone();
        let chat_streams = ChatStreamLimiter::new(DEFAULT_CHAT_STREAMS_PER_ACCOUNT);
        // This harness runs no fleet gate; the deploy-gating suite owns
        // readiness behavior.
        let readiness = Readiness::ready(control.clone());
        let quota_billing = QuotaBilling::for_tests(control.clone(), BASE_DOMAIN)
            .await
            .expect("plans");
        let server = HttpServer::new(move || {
            App::new().configure(configure_cloud_app(
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
            ))
        })
        .workers(1)
        .listen(listener)
        .expect("attach listener")
        .run();
        let handle = server.handle();
        actix_web::rt::spawn(server);

        ProviderHarness {
            control,
            cluster,
            cache,
            managed,
            api,
            managed_mock,
            sender,
            // Completion 302s point at unresolvable hosts; never follow.
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("build http client"),
            base_url: format!("http://127.0.0.1:{port}"),
            handle,
            bodies: Mutex::new(Vec::new()),
            _fallback: fallback,
        }
    }

    async fn stop(self) {
        self.handle.stop(false).await;
    }

    /// Request builder addressed at `subdomain.<BASE_DOMAIN>` (via explicit
    /// `Host` header) over the loopback listener. Caller attaches auth.
    fn api(&self, method: Method, subdomain: &str, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base_url))
            .header(HOST, format!("{subdomain}.{BASE_DOMAIN}"))
    }

    /// Read a response's status and JSON body, recording the raw body for
    /// the end-of-test key-material scan.
    async fn read(&self, resp: reqwest::Response) -> (StatusCode, Value) {
        let status = resp.status();
        let text = resp.text().await.expect("read body");
        self.bodies.lock().expect("bodies lock").push(text.clone());
        let body = serde_json::from_str(&text).unwrap_or(Value::Null);
        (status, body)
    }

    /// Assert no collected response body contains `needle`.
    fn assert_bodies_free_of(&self, needle: &str, label: &str) {
        for body in self.bodies.lock().expect("bodies lock").iter() {
            assert!(
                !body.contains(needle),
                "{label} leaked into a response body: {body}"
            );
        }
    }

    /// Full HTTP signup on the live composition: request-link on the app
    /// host → captured magic link → completion (which provisions the
    /// account, managed key included, and sets the session cookie).
    /// Returns the account plus its session secret.
    async fn signup(&self, email: &str, subdomain: &str) -> (String, String) {
        let resp = self
            .client
            .request(
                Method::POST,
                format!("{}/signup/request-link", self.base_url),
            )
            .header(HOST, format!("app.{BASE_DOMAIN}"))
            .json(&json!({ "email": email, "subdomain": subdomain }))
            .send()
            .await
            .expect("send signup request-link");
        assert_eq!(resp.status(), StatusCode::OK, "request signup link");

        let sent = self.sender.sent();
        let link = &sent.last().expect("captured signup email").link;
        let token = link.split("token=").nth(1).expect("link carries a token");

        let resp = self
            .client
            .request(
                Method::GET,
                format!("{}/signup/complete?token={token}", self.base_url),
            )
            .header(HOST, format!("app.{BASE_DOMAIN}"))
            .send()
            .await
            .expect("send signup complete");
        assert_eq!(resp.status(), StatusCode::FOUND, "signup completion");
        let location = resp
            .headers()
            .get(LOCATION)
            .expect("redirect target")
            .to_str()
            .expect("ascii location");
        assert!(
            location.contains(&format!("{subdomain}.{BASE_DOMAIN}")),
            "redirect goes to the new subdomain: {location}"
        );
        let session = resp
            .headers()
            .get(SET_COOKIE)
            .expect("session cookie")
            .to_str()
            .expect("ascii cookie")
            .strip_prefix(&format!("{SESSION_COOKIE}="))
            .expect("session cookie name")
            .split(';')
            .next()
            .expect("cookie value")
            .to_string();

        let account_id = self
            .control
            .account_id_by_subdomain(subdomain)
            .await
            .expect("look up account")
            .expect("account exists after signup");
        (account_id, session)
    }

    /// Direct provisioning through the library (the signup tests cover the
    /// HTTP path), plus an account-scope token.
    async fn provision(&self, subdomain: &str) -> (ProvisionedAccount, String) {
        let account = provision_account(
            &self.control,
            &self.cluster,
            &self.managed,
            NewAccount {
                email: format!("{subdomain}@example.com"),
                subdomain: subdomain.to_string(),
            },
        )
        .await
        .expect("provision account");
        let token = issue_token(
            &self.control,
            &account.account_id,
            TokenScope::Account,
            None,
            "provider-e2e",
        )
        .await
        .expect("issue account token");
        (account, token)
    }

    async fn create_atom(&self, subdomain: &str, token: &str, content: &str) -> String {
        let resp = self
            .api(Method::POST, subdomain, "/api/atoms")
            .bearer_auth(token)
            .json(&json!({ "content": content }))
            .send()
            .await
            .expect("send create atom");
        let (status, body) = self.read(resp).await;
        assert_eq!(status, StatusCode::CREATED, "create atom: {body}");
        body["id"].as_str().expect("atom id").to_string()
    }

    /// Poll the atom until its embedding pipeline reaches a terminal state,
    /// returning the final atom JSON.
    async fn poll_pipeline_done(&self, subdomain: &str, token: &str, atom_id: &str) -> Value {
        let deadline = std::time::Instant::now() + PIPELINE_DEADLINE;
        loop {
            let resp = self
                .api(Method::GET, subdomain, &format!("/api/atoms/{atom_id}"))
                .bearer_auth(token)
                .send()
                .await
                .expect("send get atom");
            assert_eq!(resp.status(), StatusCode::OK, "atom exists while polling");
            let body: Value = resp.json().await.expect("atom json");
            let status = body["embedding_status"].as_str().unwrap_or("");
            if matches!(status, "complete" | "failed" | "skipped") {
                return body;
            }
            if std::time::Instant::now() >= deadline {
                panic!("pipeline for {atom_id} not terminal in {PIPELINE_DEADLINE:?}: {status:?}");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Create an atom and require its embedding to COMPLETE.
    async fn embed_atom(&self, subdomain: &str, token: &str, content: &str) {
        let atom_id = self.create_atom(subdomain, token, content).await;
        let atom = self.poll_pipeline_done(subdomain, token, &atom_id).await;
        assert_eq!(
            atom["embedding_status"], "complete",
            "embedding must complete: {atom}"
        );
    }
}

/// The active `(provider, origin)` pointer on the accounts row.
async fn active_pointer(control: &ControlPlane, account_id: &str) -> Option<(String, String)> {
    sqlx::query_as(
        "SELECT active_provider, active_origin FROM accounts \
         WHERE id = $1 AND active_provider IS NOT NULL",
    )
    .bind(account_id)
    .fetch_optional(control.pool())
    .await
    .expect("read active pointer")
}

/// Number of `provider_credentials` rows for an account with `origin`.
async fn rows_with_origin(control: &ControlPlane, account_id: &str, origin: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM provider_credentials WHERE account_id = $1 AND origin = $2",
    )
    .bind(account_id)
    .bind(origin)
    .fetch_one(control.pool())
    .await
    .expect("count credential rows")
}

/// A wiremock standing in for the OpenRouter key-introspection endpoint
/// (`GET /api/v1/auth/key` — the BYOK validation call), answering `status`
/// with `body` only when the bearer header carries `expected_key`.
async fn openrouter_auth_endpoint(expected_key: &str, status: u16, body: Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/auth/key"))
        .and(header("authorization", format!("Bearer {expected_key}")))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(&server)
        .await;
    server
}

// ==================== Tests ====================

/// The headline plumbing proof: a full HTTP signup provisions a managed key
/// (recorded, never real), and the tenant's atom-create pipeline embeds via
/// the MANAGED config — pointed at the managed `MockAiServer` through the
/// platform-side `openrouter_base_url` override — while the tenant
/// database's own settings table is pre-seeded with a *different, dead*
/// provider config. The pipeline completing proves provider resolution is
/// control-plane-only: the settings-table config (a dead port) would fail
/// instantly if consulted (plan: "Plumbing — control plane → AtomicCore").
#[actix_web::test]
async fn managed_signup_embeds_via_control_plane_config_only() {
    with_control_db(
        "managed_signup_embeds_via_control_plane_config_only",
        |url| async move {
            let h = ProviderHarness::spawn_managed(&url).await;
            let (account_id, session) = h.signup("alpha@example.com", "alpha").await;

            // Exactly one managed key was minted, through the recording API.
            assert_eq!(h.api.creates().len(), 1, "one managed key at signup");
            let (managed_plaintext, _key_id) = RecordingProvisioning::nth_key(0);

            // Poison the tenant settings table with a dead provider config
            // BEFORE any authenticated request builds the cache entry. If
            // the registry-fallback path were ever consulted, the pipeline
            // would try a dead port and fail.
            let db_name: String =
                sqlx::query_scalar("SELECT db_name FROM account_databases WHERE account_id = $1")
                    .bind(&account_id)
                    .fetch_one(h.control.pool())
                    .await
                    .expect("tenant db name");
            let tenant_url = h.cluster.tenant_db_url(&db_name).expect("tenant url");
            let manager = DatabaseManager::new_postgres(".", &tenant_url)
                .await
                .expect("open tenant manager");
            let core = manager.active_core().await.expect("active core");
            for (k, v) in [
                ("provider", "openai_compat"),
                ("openai_compat_base_url", "http://127.0.0.1:9"),
                ("openai_compat_api_key", "dead-settings-key"),
                ("openai_compat_embedding_model", "dead-embed"),
            ] {
                core.set_setting(k, v).await.expect("seed dead setting");
            }
            drop(manager);

            // Atom create through the live server, authenticated by the
            // signup session: the pipeline must embed via the managed mock.
            let token = issue_token(&h.control, &account_id, TokenScope::Account, None, "e2e")
                .await
                .expect("issue token");
            let before = h.managed_mock.embedding_request_count();
            h.embed_atom("alpha", &token, "Managed-key note about Rust workspaces.")
                .await;
            assert!(
                h.managed_mock.embedding_request_count() > before,
                "embedding must hit the managed mock"
            );

            // Status (session-authenticated): managed + configured, with
            // best-effort usage from the recording API, and not a whiff of
            // key material.
            let resp = h
                .api(Method::GET, "alpha", "/api/account/provider")
                .header("Cookie", format!("{SESSION_COOKIE}={session}"))
                .send()
                .await
                .expect("send status");
            let (status, body) = h.read(resp).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["configured"], true);
            assert_eq!(body["provider"], "openrouter");
            assert_eq!(body["origin"], "managed");
            assert_eq!(
                body["model_config"]["embedding_model"],
                "openai/text-embedding-3-small"
            );
            assert_eq!(
                body["usage"]["limit_usd"], 0.5,
                "usage from get_key_usage: {body}"
            );
            assert_eq!(body["usage"]["disabled"], false);
            assert!(
                body["last_used_at"].is_string(),
                "serving the tenant stamps last_used_at: {body}"
            );

            // The managed plaintext is encrypted at rest and absent from
            // every response body; same for the master key in both common
            // encodings.
            assert!(
                !control_db_contains(&url, &managed_plaintext).await,
                "managed key plaintext must never be at rest"
            );
            h.assert_bodies_free_of(&managed_plaintext, "managed key plaintext");
            let master_hex = data_encoding::HEXLOWER.encode(&TEST_MASTER_KEY);
            let master_b64 = data_encoding::BASE64.encode(&TEST_MASTER_KEY);
            h.assert_bodies_free_of(&master_hex, "master key (hex)");
            h.assert_bodies_free_of(&master_b64, "master key (base64)");

            h.stop().await;
        },
    )
    .await;
}

/// BYOK validation failure (plan: "BYOK entry & validation" — reject the
/// save, surface the provider's error verbatim): a 401 from the key
/// endpoint produces a 400 carrying the provider's message, stores nothing
/// (no user row, plaintext never at rest, active pointer untouched), and
/// the account keeps embedding through its managed key. The success leg of
/// the same wiremock pins the real validation request shape — bearer auth
/// against `GET {base}/v1/auth/key`.
#[actix_web::test]
async fn byok_validation_failure_stores_nothing() {
    with_control_db("byok_validation_failure_stores_nothing", |url| async move {
        let h = ProviderHarness::spawn_managed(&url).await;
        let (account, token) = h.provision("alpha").await;
        let account_id = &account.account_id;

        let bad_endpoint = openrouter_auth_endpoint(
            "sk-or-bad-key",
            401,
            json!({ "error": { "message": "Invalid API key: mock says no" } }),
        )
        .await;

        let resp = h
            .api(Method::PUT, "alpha", "/api/account/provider")
            .bearer_auth(&token)
            .json(&json!({
                "provider": "openrouter",
                "api_key": "sk-or-bad-key",
                "model_config": { "openrouter_base_url": bad_endpoint.uri() },
            }))
            .send()
            .await
            .expect("send byok put");
        let (status, body) = h.read(resp).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "provider_validation_failed");
        let message = body["message"].as_str().expect("message");
        assert!(message.contains("401"), "carries the status: {message}");
        assert!(
            message.contains("mock says no"),
            "carries the provider's error verbatim: {message}"
        );

        // Nothing stored, active pointer untouched, no plaintext anywhere.
        assert_eq!(rows_with_origin(&h.control, account_id, "user").await, 0);
        assert_eq!(
            active_pointer(&h.control, account_id).await,
            Some(("openrouter".to_string(), "managed".to_string()))
        );
        assert!(!control_db_contains(&url, "sk-or-bad-key").await);
        h.assert_bodies_free_of("sk-or-bad-key", "rejected BYOK key");

        // The account still embeds through the managed config.
        let before = h.managed_mock.embedding_request_count();
        h.embed_atom("alpha", &token, "Still managed after the failed save.")
            .await;
        assert!(h.managed_mock.embedding_request_count() > before);

        // Success leg: a 200 from the auth endpoint accepts the save — this
        // pins the validation client's request shape (the matcher requires
        // the bearer header) against a wiremock, NO REAL PROVIDERS.
        let good_endpoint = openrouter_auth_endpoint(
            "sk-or-good-key",
            200,
            json!({ "data": { "label": "mock", "usage": 0 } }),
        )
        .await;
        let resp = h
            .api(Method::PUT, "alpha", "/api/account/provider")
            .bearer_auth(&token)
            .json(&json!({
                "provider": "openrouter",
                "api_key": "sk-or-good-key",
                "model_config": { "openrouter_base_url": good_endpoint.uri() },
            }))
            .send()
            .await
            .expect("send byok put");
        let (status, body) = h.read(resp).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "saved");
        assert_eq!(body["origin"], "user");
        assert_eq!(
            body["reembed_warning"],
            Value::Null,
            "same embedding model, no warning: {body}"
        );
        assert_eq!(rows_with_origin(&h.control, account_id, "user").await, 1);
        assert!(!control_db_contains(&url, "sk-or-good-key").await);
        h.assert_bodies_free_of("sk-or-good-key", "accepted BYOK key");

        h.stop().await;
    })
    .await;
}

/// Live rotation (plan steps 1-5): a successful BYOK save swaps the cached
/// entry's provider config IN PLACE — same manager, no eviction — so the
/// very next embed hits the BYOK mock; activating the managed row flips
/// back the same way. Also: activation of a nonexistent row 404s without
/// touching anything, and switching to a different embedding model carries
/// the loud re-embed warning.
#[actix_web::test]
async fn byok_save_rotates_live_without_eviction() {
    with_control_db(
        "byok_save_rotates_live_without_eviction",
        |url| async move {
            let h = ProviderHarness::spawn_managed(&url).await;
            let (account, token) = h.provision("alpha").await;
            let account_id = &account.account_id;

            // Warm the cache through the managed config and keep the handle for
            // the identity assertion.
            h.embed_atom("alpha", &token, "First note, embedded via managed.")
                .await;
            let handle_before = h.cache.get_or_load(account_id).await.expect("cached entry");

            // BYOK save: an OpenAI-compatible endpoint (the BYOK MockAiServer).
            // Validation runs a real embedding call through the same provider
            // machinery the pipeline uses, so the BYOK mock's counter proves it.
            let byok_mock = MockAiServer::start().await;
            let resp = h
                .api(Method::PUT, "alpha", "/api/account/provider")
                .bearer_auth(&token)
                .json(&json!({
                    "provider": "openai_compat",
                    "api_key": "sk-byok-compat-secret",
                    "model_config": {
                        "embedding_model": "mock-embed",
                        "llm_model": "mock-llm",
                        "openai_compat_base_url": byok_mock.base_url(),
                        "embedding_dimension": 1536,
                    },
                }))
                .send()
                .await
                .expect("send byok put");
            let (status, body) = h.read(resp).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(
                byok_mock.embedding_request_count(),
                1,
                "validation made one embedding call against the BYOK endpoint"
            );
            // The embedding model changed (pinned managed model → mock-embed):
            // the loud re-embed warning must ride along.
            assert!(
                body["reembed_warning"]
                    .as_str()
                    .is_some_and(|w| w.contains("re-embed")),
                "embedding-model change carries the warning: {body}"
            );
            assert_eq!(
                active_pointer(&h.control, account_id).await,
                Some(("openai_compat".to_string(), "user".to_string()))
            );

            // No eviction: the exact same manager serves, with the new config.
            let handle_after = h.cache.get_or_load(account_id).await.expect("cached entry");
            assert!(
                Arc::ptr_eq(&handle_before.manager, &handle_after.manager),
                "live rotation must not evict or rebuild the cache entry"
            );

            // Next embed hits the BYOK mock, not the managed one.
            let managed_before = h.managed_mock.embedding_request_count();
            let byok_before = byok_mock.embedding_request_count();
            h.embed_atom("alpha", &token, "Second note, embedded via BYOK.")
                .await;
            assert!(
                byok_mock.embedding_request_count() > byok_before,
                "post-rotation embeds go to the BYOK endpoint"
            );
            assert_eq!(
                h.managed_mock.embedding_request_count(),
                managed_before,
                "the managed endpoint sees nothing after rotation"
            );

            // Activating a row that doesn't exist: 404, nothing changes. The
            // managed row is (openrouter, managed) — (openai_compat, managed)
            // was never provisioned.
            let resp = h
                .api(Method::POST, "alpha", "/api/account/provider/activate")
                .bearer_auth(&token)
                .json(&json!({ "provider": "openai_compat", "origin": "managed" }))
                .send()
                .await
                .expect("send activate");
            let (status, body) = h.read(resp).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
            assert_eq!(body["error"], "provider_credentials_not_found");
            assert_eq!(
                active_pointer(&h.control, account_id).await,
                Some(("openai_compat".to_string(), "user".to_string()))
            );

            // Activate back to managed: the column flip, live again, same entry.
            let resp = h
                .api(Method::POST, "alpha", "/api/account/provider/activate")
                .bearer_auth(&token)
                .json(&json!({ "provider": "openrouter", "origin": "managed" }))
                .send()
                .await
                .expect("send activate");
            let (status, body) = h.read(resp).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["status"], "activated");
            assert!(
                body["reembed_warning"].as_str().is_some(),
                "switching back also changes the embedding model: {body}"
            );

            let managed_before = h.managed_mock.embedding_request_count();
            let byok_before = byok_mock.embedding_request_count();
            h.embed_atom("alpha", &token, "Third note, embedded via managed again.")
                .await;
            assert!(
                h.managed_mock.embedding_request_count() > managed_before,
                "post-flip embeds go back to the managed endpoint"
            );
            assert_eq!(byok_mock.embedding_request_count(), byok_before);
            let handle_final = h.cache.get_or_load(account_id).await.expect("cached entry");
            assert!(
                Arc::ptr_eq(&handle_before.manager, &handle_final.manager),
                "both rotations left the entry in place"
            );

            // The BYOK plaintext: encrypted at rest, never in a body. The
            // stored key is never displayed — confirm against the status route
            // too.
            let resp = h
                .api(Method::GET, "alpha", "/api/account/provider")
                .bearer_auth(&token)
                .send()
                .await
                .expect("send status");
            let (status, _body) = h.read(resp).await;
            assert_eq!(status, StatusCode::OK);
            assert!(!control_db_contains(&url, "sk-byok-compat-secret").await);
            h.assert_bodies_free_of("sk-byok-compat-secret", "BYOK key plaintext");
            let (managed_plaintext, _) = RecordingProvisioning::nth_key(0);
            h.assert_bodies_free_of(&managed_plaintext, "managed key plaintext");

            h.stop().await;
        },
    )
    .await;
}

/// Model curation (plan: "Model curation"): managed writes are pinned to
/// the curated set — uncurated LLMs, foreign embedding models, and (the
/// exfiltration vector) base-URL overrides are all 400s that change nothing
/// — while a curated choice lands. BYOK writes are free, with the loud
/// re-embed warning when (and only when) the embedding model changes.
#[actix_web::test]
async fn model_curation_managed_pinned_byok_free() {
    with_control_db(
        "model_curation_managed_pinned_byok_free",
        |url| async move {
            let h = ProviderHarness::spawn_managed(&url).await;
            let (account, token) = h.provision("alpha").await;
            let account_id = &account.account_id;

            // Managed origin: every uncurated write refused.
            for (label, model_config) in [
                ("uncurated llm", json!({ "llm_model": "openai/o1-pro" })),
                (
                    "foreign embedding model",
                    json!({ "embedding_model": "openai/text-embedding-3-large" }),
                ),
                (
                    "base-url exfiltration",
                    json!({ "openrouter_base_url": "https://attacker.example/api/v1" }),
                ),
            ] {
                let resp = h
                    .api(Method::PUT, "alpha", "/api/account/provider/models")
                    .bearer_auth(&token)
                    .json(&json!({ "model_config": model_config }))
                    .send()
                    .await
                    .expect("send models put");
                let (status, body) = h.read(resp).await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{label}: {body}");
                assert_eq!(body["error"], "model_not_curated", "{label}");
            }
            // Refused writes changed nothing: the stored config still carries
            // the signup seed (including the platform-side base-URL override).
            let stored: Value = sqlx::query_scalar(
                "SELECT model_config FROM provider_credentials \
             WHERE account_id = $1 AND origin = 'managed'",
            )
            .bind(account_id)
            .fetch_one(h.control.pool())
            .await
            .expect("read stored model config");
            assert_eq!(stored, managed_model_config(&h.managed_mock));

            // A curated choice lands; same embedding model, so no warning.
            let curated = MANAGED_LLM_MODELS[1];
            let resp = h
                .api(Method::PUT, "alpha", "/api/account/provider/models")
                .bearer_auth(&token)
                .json(&json!({ "model_config": {
                    "embedding_model": "openai/text-embedding-3-small",
                    "llm_model": curated,
                }}))
                .send()
                .await
                .expect("send models put");
            let (status, body) = h.read(resp).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["status"], "updated");
            assert_eq!(body["model_config"]["llm_model"], curated);
            assert_eq!(body["reembed_warning"], Value::Null, "{body}");
            // The managed write MERGES over the stored config: the user can
            // only submit the curated model keys, so the platform-seeded
            // base-URL override (which curation forbids them from
            // resubmitting) must survive the write — both in storage and in
            // the response's echo of the effective config.
            let mut expected = managed_model_config(&h.managed_mock);
            expected["llm_model"] = json!(curated);
            assert_eq!(
                body["model_config"], expected,
                "the response echoes the merged config"
            );
            let stored: Value = sqlx::query_scalar(
                "SELECT model_config FROM provider_credentials \
             WHERE account_id = $1 AND origin = 'managed'",
            )
            .bind(account_id)
            .fetch_one(h.control.pool())
            .await
            .expect("read stored model config");
            assert_eq!(
                stored, expected,
                "platform-seeded keys must survive a curated models write"
            );
            // And the preserved override is live, not just stored: a managed
            // embed still lands on the managed mock, not the real endpoint.
            let before = h.managed_mock.embedding_request_count();
            h.embed_atom("alpha", &token, "Embedded after the curated models write.")
                .await;
            assert!(
                h.managed_mock.embedding_request_count() > before,
                "post-write managed embeds must still hit the managed mock"
            );

            // BYOK origin: anything goes. Save a key against the BYOK mock,
            // then pick arbitrary models — a non-embedding change is silent, an
            // embedding change warns loudly.
            let byok_mock = MockAiServer::start().await;
            let resp = h
                .api(Method::PUT, "alpha", "/api/account/provider")
                .bearer_auth(&token)
                .json(&json!({
                    "provider": "openai_compat",
                    "api_key": "sk-byok-free",
                    "model_config": {
                        "embedding_model": "mock-embed",
                        "llm_model": "mock-llm",
                        "openai_compat_base_url": byok_mock.base_url(),
                    },
                }))
                .send()
                .await
                .expect("send byok put");
            let (status, body) = h.read(resp).await;
            assert_eq!(status, StatusCode::OK, "{body}");

            let resp = h
                .api(Method::PUT, "alpha", "/api/account/provider/models")
                .bearer_auth(&token)
                .json(&json!({ "model_config": {
                    "embedding_model": "mock-embed",
                    "llm_model": "any/model-i-like",
                    "openai_compat_base_url": byok_mock.base_url(),
                }}))
                .send()
                .await
                .expect("send models put");
            let (status, body) = h.read(resp).await;
            assert_eq!(status, StatusCode::OK, "BYOK models are free: {body}");
            assert_eq!(body["reembed_warning"], Value::Null, "{body}");

            let resp = h
                .api(Method::PUT, "alpha", "/api/account/provider/models")
                .bearer_auth(&token)
                .json(&json!({ "model_config": {
                    "embedding_model": "mock-embed-v2",
                    "llm_model": "any/model-i-like",
                    "openai_compat_base_url": byok_mock.base_url(),
                }}))
                .send()
                .await
                .expect("send models put");
            let (status, body) = h.read(resp).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert!(
                body["reembed_warning"]
                    .as_str()
                    .is_some_and(|w| w.contains("re-embed")),
                "BYOK embedding change warns loudly: {body}"
            );

            h.stop().await;
        },
    )
    .await;
}

/// The curation-bypass closure (plan: "Model curation"): wiki, chat, and
/// report models resolve from the tenant-writable `wiki_model`/`chat_model`
/// settings keys, and `PUT /api/settings/{key}` is live on cloud tenant
/// hosts — so without atomic-core's explicit-mode overlay
/// (`ProviderConfig::apply_to_settings` pins both keys to the config's
/// `llm_model`), a managed tenant could route frontier inference onto the
/// platform-funded key with one settings write. This pins the closure
/// end-to-end: the settings writes succeed (they're inert, not blocked),
/// but every LLM request reaching the managed mock still carries the
/// curated managed model. Remove the overlay and this test fails with the
/// frontier model in the request body.
#[actix_web::test]
async fn settings_writes_cannot_reroute_managed_llm_traffic() {
    with_control_db(
        "settings_writes_cannot_reroute_managed_llm_traffic",
        |url| async move {
            let h = ProviderHarness::spawn_managed(&url).await;
            let (_account, token) = h.provision("alpha").await;
            let curated_llm = managed_model_config(&h.managed_mock)["llm_model"]
                .as_str()
                .expect("seeded llm model")
                .to_string();

            // The tenant writes frontier models into the per-task keys. The
            // writes succeed — settings stay writable; the overlay makes the
            // provider-model keys inert rather than rejecting them.
            const FRONTIER: &str = "frontier/extremely-expensive";
            for key in ["chat_model", "wiki_model"] {
                let resp = h
                    .api(Method::PUT, "alpha", &format!("/api/settings/{key}"))
                    .bearer_auth(&token)
                    .json(&json!({ "value": FRONTIER }))
                    .send()
                    .await
                    .expect("send settings put");
                let (status, body) = h.read(resp).await;
                assert_eq!(status, StatusCode::OK, "{key}: {body}");
            }

            // A chat round-trip through the live server — the cheapest
            // operation that resolves its model from `chat_model`.
            let resp = h
                .api(Method::POST, "alpha", "/api/conversations")
                .bearer_auth(&token)
                .json(&json!({ "tag_ids": [], "title": "curation bypass" }))
                .send()
                .await
                .expect("send create conversation");
            let (status, conversation) = h.read(resp).await;
            assert_eq!(status, StatusCode::CREATED, "{conversation}");
            let conversation_id = conversation["id"].as_str().expect("conversation id");

            let resp = h
                .api(
                    Method::POST,
                    "alpha",
                    &format!("/api/conversations/{conversation_id}/messages"),
                )
                .bearer_auth(&token)
                .json(&json!({ "content": "What do my notes say about Rust workspaces?" }))
                .send()
                .await
                .expect("send chat message");
            let (status, message) = h.read(resp).await;
            assert_eq!(status, StatusCode::OK, "{message}");

            // Every LLM request that reached the managed endpoint carried
            // the curated model — in explicit mode the per-task keys are
            // pinned to the config's `llm_model`, so nothing else can occur.
            let models = h.managed_mock.chat_request_models();
            assert!(
                !models.is_empty(),
                "the chat round-trip must produce LLM traffic on the managed mock"
            );
            for model in &models {
                assert_eq!(
                    model, &curated_llm,
                    "managed LLM traffic must carry the curated model, \
                     never the settings-written one: {models:?}"
                );
            }

            h.stop().await;
        },
    )
    .await;
}

/// The key-echo scrub (module docs in `tenant_plane`: never include the key
/// in error messages): a hostile or misconfigured validation endpoint that
/// echoes the submitted key verbatim in its error body must produce a 400
/// whose message carries `[redacted]` — with the surrounding provider
/// context intact — and no response body anywhere containing the key.
#[actix_web::test]
async fn byok_validation_error_scrubs_echoed_key() {
    with_control_db(
        "byok_validation_error_scrubs_echoed_key",
        |url| async move {
            let h = ProviderHarness::spawn_managed(&url).await;
            let (_account, token) = h.provision("alpha").await;

            const ECHOED_KEY: &str = "sk-or-echo-victim-3cf09a";
            let hostile = openrouter_auth_endpoint(
                ECHOED_KEY,
                401,
                json!({ "error": {
                    "message": format!("Invalid key {ECHOED_KEY} rejected by hostile endpoint")
                }}),
            )
            .await;

            let resp = h
                .api(Method::PUT, "alpha", "/api/account/provider")
                .bearer_auth(&token)
                .json(&json!({
                    "provider": "openrouter",
                    "api_key": ECHOED_KEY,
                    "model_config": { "openrouter_base_url": hostile.uri() },
                }))
                .send()
                .await
                .expect("send byok put");
            let (status, body) = h.read(resp).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(body["error"], "provider_validation_failed");
            let message = body["message"].as_str().expect("message");
            assert!(
                message.contains("[redacted]"),
                "the echo is scrubbed to a marker: {message}"
            );
            assert!(
                !message.contains(ECHOED_KEY),
                "the submitted key must not survive the scrub: {message}"
            );
            assert!(
                message.contains("rejected by hostile endpoint"),
                "the provider's error context survives around the scrub: {message}"
            );

            // Belt and braces: not in any body this harness has read, and the
            // rejected save stored nothing.
            h.assert_bodies_free_of(ECHOED_KEY, "echoed BYOK key");
            assert!(!control_db_contains(&url, ECHOED_KEY).await);

            h.stop().await;
        },
    )
    .await;
}

/// Scope gating, same idiom as `DELETE /api/account`: database- and
/// MCP-scoped tokens get 403 on every provider route — a KB-pinned
/// integration must not read or rotate the account's provider credentials —
/// and none of the routes exist on the app host.
#[actix_web::test]
async fn provider_routes_require_account_scope() {
    with_control_db("provider_routes_require_account_scope", |url| async move {
        let h = ProviderHarness::spawn_managed(&url).await;
        let (account, _token) = h.provision("alpha").await;

        let routes = [
            (Method::GET, "/api/account/provider"),
            (Method::PUT, "/api/account/provider"),
            (Method::POST, "/api/account/provider/activate"),
            (Method::PUT, "/api/account/provider/models"),
        ];

        for (scope, name) in [
            (TokenScope::Database, "database-scoped"),
            (TokenScope::Mcp, "mcp-scoped"),
        ] {
            let scoped = issue_token(
                &h.control,
                &account.account_id,
                scope,
                Some("default"),
                name,
            )
            .await
            .expect("issue scoped token");
            for (method, route) in &routes {
                let resp = h
                    .api(method.clone(), "alpha", route)
                    .bearer_auth(&scoped)
                    // A well-formed body, so the refusal can only be scope.
                    .json(&json!({
                        "provider": "openrouter",
                        "origin": "managed",
                        "api_key": "k",
                        "model_config": {},
                    }))
                    .send()
                    .await
                    .expect("send scoped request");
                let (status, body) = h.read(resp).await;
                assert_eq!(
                    status,
                    StatusCode::FORBIDDEN,
                    "{name} {method} {route}: {body}"
                );
                assert_eq!(
                    body["error"], "account_scope_required",
                    "{name} {method} {route}"
                );
            }
        }

        // The routes don't exist on the app plane.
        for (method, route) in &routes {
            let resp = h
                .client
                .request(method.clone(), format!("{}{route}", h.base_url))
                .header(HOST, format!("app.{BASE_DOMAIN}"))
                .send()
                .await
                .expect("send app-host request");
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "{method} {route} must not exist on the app host"
            );
        }

        h.stop().await;
    })
    .await;
}

/// The keyless state (provisioning disabled, no credentials row): atom
/// creation succeeds — the pipeline is fire-and-forget — and the pipeline
/// then fails with atomic-core's structured missing-provider error (pinned
/// to today's behavior, not invented: the batch pipeline's "provider was
/// not configured" sweep marks the atom failed when the key-less config
/// can't build a provider), because the cache passed an explicit key-less
/// config rather than falling back to tenant settings. The status route
/// reports unconfigured.
#[actix_web::test]
async fn keyless_account_reports_structured_missing_key_failure() {
    with_control_db(
        "keyless_account_reports_structured_missing_key_failure",
        |url| async move {
            let h = ProviderHarness::spawn_disabled(&url).await;
            let (account, token) = h.provision("alpha").await;
            assert_eq!(
                active_pointer(&h.control, &account.account_id).await,
                None,
                "disabled mode provisions no credentials"
            );

            // Creation succeeds; the pipeline fails structurally.
            let atom_id = h
                .create_atom("alpha", &token, "A note with nowhere to embed.")
                .await;
            let atom = h.poll_pipeline_done("alpha", &token, &atom_id).await;
            assert_eq!(atom["embedding_status"], "failed", "{atom}");
            let error = atom["embedding_error"].as_str().expect("embedding_error");
            assert!(
                error.contains("provider was not configured"),
                "the structured missing-provider error, not a connection error: {error}"
            );

            // The atom itself is intact and listed.
            let resp = h
                .api(Method::GET, "alpha", "/api/atoms")
                .bearer_auth(&token)
                .send()
                .await
                .expect("send list");
            let (status, body) = h.read(resp).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["total_count"], 1, "{body}");

            // Status: honest about the unconfigured state.
            let resp = h
                .api(Method::GET, "alpha", "/api/account/provider")
                .bearer_auth(&token)
                .send()
                .await
                .expect("send status");
            let (status, body) = h.read(resp).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["configured"], false);
            assert_eq!(body["provider"], Value::Null);
            assert_eq!(body["usage"], Value::Null);

            h.stop().await;
        },
    )
    .await;
}

/// Malformed provider-route requests are structured 400s: unknown
/// providers (Ollama is not available in cloud), empty keys, non-object
/// model configs, missing bodies, unknown origins — and a models write with
/// no provider configured is a 404.
#[actix_web::test]
async fn provider_route_input_validation() {
    with_control_db("provider_route_input_validation", |url| async move {
        let h = ProviderHarness::spawn_disabled(&url).await;
        let (_account, token) = h.provision("alpha").await;

        let cases: [(Method, &str, Value, &str); 6] = [
            (
                Method::PUT,
                "/api/account/provider",
                json!({ "provider": "ollama", "api_key": "k" }),
                "invalid_provider",
            ),
            (
                Method::PUT,
                "/api/account/provider",
                json!({ "provider": "openrouter", "api_key": "   " }),
                "invalid_api_key",
            ),
            (
                Method::PUT,
                "/api/account/provider",
                json!({ "provider": "openrouter", "api_key": "k", "model_config": ["x"] }),
                "invalid_model_config",
            ),
            (
                Method::POST,
                "/api/account/provider/activate",
                json!({ "provider": "openrouter", "origin": "platform" }),
                "invalid_origin",
            ),
            (
                Method::POST,
                "/api/account/provider/activate",
                Value::Null, // sent as a bodyless request below
                "invalid_request",
            ),
            (
                Method::PUT,
                "/api/account/provider/models",
                json!({ "model_config": "gpt" }),
                "invalid_model_config",
            ),
        ];
        for (method, route, body, expected_error) in cases {
            let mut request = h.api(method.clone(), "alpha", route).bearer_auth(&token);
            if !body.is_null() {
                request = request.json(&body);
            }
            let resp = request.send().await.expect("send malformed request");
            let (status, response) = h.read(resp).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{method} {route}: {response}"
            );
            assert_eq!(response["error"], expected_error, "{method} {route}");
        }

        // Models write with nothing configured: 404, not a silent no-op.
        let resp = h
            .api(Method::PUT, "alpha", "/api/account/provider/models")
            .bearer_auth(&token)
            .json(&json!({ "model_config": {} }))
            .send()
            .await
            .expect("send models put");
        let (status, body) = h.read(resp).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error"], "no_active_provider");

        h.stop().await;
    })
    .await;
}

/// The embedding-space closure of the curation bypass (atomic-core's
/// explicit mode, embedding-space half): `PUT /api/settings/{key}` is live
/// on cloud tenant hosts, and `provider` / `openai_compat_embedding_dimension`
/// are embedding-space keys routed through `set_setting_with_reembed`. In
/// explicit mode those writes must be INERT — stored, but never recreating
/// the tenant's vector index at a dimension the managed config doesn't
/// produce (which would destroy every stored vector) and never queueing a
/// platform-billed re-embed. Pinned end-to-end: the writes succeed and
/// report no space change, a later embed still completes at the managed
/// dimension, and the atom embedded BEFORE the writes still answers
/// semantic search — its vectors survived.
#[actix_web::test]
async fn settings_writes_cannot_change_embedding_space() {
    with_control_db(
        "settings_writes_cannot_change_embedding_space",
        |url| async move {
            let h = ProviderHarness::spawn_managed(&url).await;
            let (_account, token) = h.provision("alpha").await;

            // An atom embedded through the managed config, before any
            // settings mischief.
            h.embed_atom("alpha", &token, "Original note about Rust workspaces.")
                .await;

            // Embedding-space settings writes: accepted (settings stay
            // writable) but inert — no recreation, no re-embed queue.
            for (key, value) in [
                ("provider", "openai_compat"),
                ("openai_compat_embedding_dimension", "3072"),
            ] {
                let resp = h
                    .api(Method::PUT, "alpha", &format!("/api/settings/{key}"))
                    .bearer_auth(&token)
                    .json(&json!({ "value": value }))
                    .send()
                    .await
                    .expect("send settings put");
                let (status, body) = h.read(resp).await;
                assert_eq!(status, StatusCode::OK, "{key}: {body}");
                assert_eq!(
                    body["embedding_space_changed"], false,
                    "{key} must be inert: {body}"
                );
                assert_eq!(
                    body["dimension_changed"], false,
                    "{key} must not recreate the index: {body}"
                );
                assert_eq!(
                    body["total_atom_count"], 0,
                    "{key} must queue no re-embeds: {body}"
                );
            }

            // Embedding still works, at the managed dimension, through the
            // managed mock.
            let before = h.managed_mock.embedding_request_count();
            h.embed_atom("alpha", &token, "Second note, embedded after the writes.")
                .await;
            assert!(
                h.managed_mock.embedding_request_count() > before,
                "post-write embeds still flow through the managed config"
            );

            // And the pre-write atom's vectors survived: semantic search
            // still returns it.
            let resp = h
                .api(Method::POST, "alpha", "/api/search")
                .bearer_auth(&token)
                .json(&json!({ "query": "Rust workspaces", "mode": "semantic" }))
                .send()
                .await
                .expect("send search");
            let (status, results) = h.read(resp).await;
            assert_eq!(status, StatusCode::OK, "{results}");
            let hits: Vec<&str> = results
                .as_array()
                .expect("array body")
                .iter()
                .filter_map(|r| r["content"].as_str())
                .collect();
            assert!(
                hits.iter().any(|c| c.contains("Original note")),
                "the pre-write atom's vectors must survive: {hits:?}"
            );

            h.stop().await;
        },
    )
    .await;
}

/// The dimension pin (v1): the tenant vector column is fixed at the
/// platform dimension and NO cloud mechanism can recreate it at another
/// width, so a BYOK save or models write whose effective embedding
/// dimension differs is a structured 400 — never stored with an
/// unfulfillable re-embed warning. Same-dimension model changes keep the
/// warning (covered here and by the rotation test).
#[actix_web::test]
async fn byok_dimension_change_is_rejected_structured() {
    with_control_db(
        "byok_dimension_change_is_rejected_structured",
        |url| async move {
            let h = ProviderHarness::spawn_managed(&url).await;
            let (account, token) = h.provision("alpha").await;
            let account_id = &account.account_id;

            // OpenAI-compat save asking for 3072: rejected before any
            // validation traffic, nothing stored, active pointer untouched.
            let byok_mock = MockAiServer::start().await;
            let resp = h
                .api(Method::PUT, "alpha", "/api/account/provider")
                .bearer_auth(&token)
                .json(&json!({
                    "provider": "openai_compat",
                    "api_key": "sk-byok-3072",
                    "model_config": {
                        "embedding_model": "mock-embed",
                        "openai_compat_base_url": byok_mock.base_url(),
                        "embedding_dimension": 3072,
                    },
                }))
                .send()
                .await
                .expect("send byok put");
            let (status, body) = h.read(resp).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(body["error"], "embedding_dimension_unsupported");
            assert_eq!(body["required_dimension"], 1536, "{body}");
            assert_eq!(body["requested_dimension"], 3072, "{body}");
            assert_eq!(
                byok_mock.embedding_request_count(),
                0,
                "the rejection must precede provider validation"
            );
            assert_eq!(rows_with_origin(&h.control, account_id, "user").await, 0);
            assert_eq!(
                active_pointer(&h.control, account_id).await,
                Some(("openrouter".to_string(), "managed".to_string()))
            );

            // OpenRouter save with a 3072-dimension model: same rejection
            // (the dimension is implied by the model, not a config key).
            let resp = h
                .api(Method::PUT, "alpha", "/api/account/provider")
                .bearer_auth(&token)
                .json(&json!({
                    "provider": "openrouter",
                    "api_key": "sk-or-large",
                    "model_config": { "embedding_model": "openai/text-embedding-3-large" },
                }))
                .send()
                .await
                .expect("send byok put");
            let (status, body) = h.read(resp).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(body["error"], "embedding_dimension_unsupported");
            assert_eq!(rows_with_origin(&h.control, account_id, "user").await, 0);

            // A pinned-dimension save with a *different model* is accepted,
            // with the loud re-embed warning.
            let resp = h
                .api(Method::PUT, "alpha", "/api/account/provider")
                .bearer_auth(&token)
                .json(&json!({
                    "provider": "openai_compat",
                    "api_key": "sk-byok-1536",
                    "model_config": {
                        "embedding_model": "mock-embed",
                        "llm_model": "mock-llm",
                        "openai_compat_base_url": byok_mock.base_url(),
                        "embedding_dimension": 1536,
                    },
                }))
                .send()
                .await
                .expect("send byok put");
            let (status, body) = h.read(resp).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert!(
                body["reembed_warning"]
                    .as_str()
                    .is_some_and(|w| w.contains("re-embed")),
                "same-dimension model change keeps the warning: {body}"
            );

            // The models route enforces the same pin, BEFORE storing.
            let resp = h
                .api(Method::PUT, "alpha", "/api/account/provider/models")
                .bearer_auth(&token)
                .json(&json!({ "model_config": {
                    "embedding_model": "mock-embed",
                    "openai_compat_base_url": byok_mock.base_url(),
                    "embedding_dimension": 3072,
                }}))
                .send()
                .await
                .expect("send models put");
            let (status, body) = h.read(resp).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(body["error"], "embedding_dimension_unsupported");
            let stored: Value = sqlx::query_scalar(
                "SELECT model_config FROM provider_credentials \
                 WHERE account_id = $1 AND origin = 'user'",
            )
            .bind(account_id)
            .fetch_one(h.control.pool())
            .await
            .expect("read stored model config");
            assert_eq!(
                stored["embedding_dimension"], 1536,
                "the rejected models write must store nothing: {stored}"
            );

            h.stop().await;
        },
    )
    .await;
}

/// The plaintext-column rule: `model_config` is stored unencrypted and
/// echoed by the status route, so BYOK writes are checked against the
/// documented vocabulary — a client nesting `api_key` inside `model_config`
/// is a structured 400 and the secret never touches storage.
#[actix_web::test]
async fn byok_model_config_rejects_unknown_keys() {
    with_control_db("byok_model_config_rejects_unknown_keys", |url| async move {
        let h = ProviderHarness::spawn_managed(&url).await;
        let (account, token) = h.provision("alpha").await;
        let account_id = &account.account_id;

        const NESTED_SECRET: &str = "sk-nested-leak-77ab2e";
        let resp = h
            .api(Method::PUT, "alpha", "/api/account/provider")
            .bearer_auth(&token)
            .json(&json!({
                "provider": "openrouter",
                "api_key": "sk-or-outer",
                "model_config": { "api_key": NESTED_SECRET },
            }))
            .send()
            .await
            .expect("send byok put");
        let (status, body) = h.read(resp).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "invalid_model_config");
        assert!(
            body["message"]
                .as_str()
                .is_some_and(|m| m.contains("api_key")),
            "the message names the offending key: {body}"
        );

        // Nothing stored — neither the row nor the nested secret.
        assert_eq!(rows_with_origin(&h.control, account_id, "user").await, 0);
        assert!(!control_db_contains(&url, NESTED_SECRET).await);
        h.assert_bodies_free_of(NESTED_SECRET, "nested model_config secret");

        // The models route applies the same vocabulary to BYOK rows. Save a
        // valid config first, then attempt the smuggle.
        let byok_mock = MockAiServer::start().await;
        let resp = h
            .api(Method::PUT, "alpha", "/api/account/provider")
            .bearer_auth(&token)
            .json(&json!({
                "provider": "openai_compat",
                "api_key": "sk-byok-ok",
                "model_config": {
                    "embedding_model": "mock-embed",
                    "openai_compat_base_url": byok_mock.base_url(),
                },
            }))
            .send()
            .await
            .expect("send byok put");
        let (status, body) = h.read(resp).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let resp = h
            .api(Method::PUT, "alpha", "/api/account/provider/models")
            .bearer_auth(&token)
            .json(&json!({ "model_config": {
                "embedding_model": "mock-embed",
                "openai_compat_base_url": byok_mock.base_url(),
                "api_key": NESTED_SECRET,
            }}))
            .send()
            .await
            .expect("send models put");
        let (status, body) = h.read(resp).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "invalid_model_config");
        assert!(!control_db_contains(&url, NESTED_SECRET).await);

        h.stop().await;
    })
    .await;
}

/// The scrub-before-truncate ordering, both provider arms: a hostile
/// endpoint that echoes the submitted key right at the truncation boundary
/// must still produce a fully scrubbed message. Scrubbing after truncation
/// would cut the echoed key mid-way and leave the surviving fragment
/// unmatched by the verbatim replace.
#[actix_web::test]
async fn echoed_key_at_truncation_boundary_is_scrubbed_on_both_arms() {
    with_control_db(
        "echoed_key_at_truncation_boundary_is_scrubbed_on_both_arms",
        |url| async move {
            let h = ProviderHarness::spawn_managed(&url).await;
            let (_account, token) = h.provision("alpha").await;

            // 44-char keys positioned so the 500-char truncation lands
            // INSIDE the echoed key — scrub-after-truncate would leave the
            // leading fragment — while the scrub marker itself stays inside
            // the bound, keeping both properties observable. Each arm's
            // filler accounts for its fixed prefixes (the JSON wrapper; the
            // compat arm's "API error (401): ").
            const OR_KEY: &str = "sk-or-boundary-echo-victim-0123456789abcdef0";
            const COMPAT_KEY: &str = "sk-cm-boundary-echo-victim-0123456789abcdef0";

            // OpenRouter arm: GET {base}/v1/auth/key answers 401 echoing the
            // key straddling the boundary (truncation applies to the body;
            // its JSON wrapper is ~21 chars, so the key spans ~481..525).
            let hostile_or = openrouter_auth_endpoint(
                OR_KEY,
                401,
                json!({ "error": { "message": format!("{}{OR_KEY} rejected", "x".repeat(460)) } }),
            )
            .await;
            let resp = h
                .api(Method::PUT, "alpha", "/api/account/provider")
                .bearer_auth(&token)
                .json(&json!({
                    "provider": "openrouter",
                    "api_key": OR_KEY,
                    "model_config": { "openrouter_base_url": hostile_or.uri() },
                }))
                .send()
                .await
                .expect("send byok put");
            let (status, body) = h.read(resp).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            let message = body["message"].as_str().expect("message");
            assert!(
                message.contains("[redacted]"),
                "the echo is scrubbed to the marker: {message}"
            );
            for fragment in [OR_KEY, &OR_KEY[..12], &OR_KEY[OR_KEY.len() - 12..]] {
                assert!(
                    !message.contains(fragment),
                    "no fragment of the key may survive the boundary: {message}"
                );
            }

            // OpenAI-compat arm: the validation embedding call hits
            // POST {base}/v1/embeddings; answer 401 echoing the key at the
            // boundary. This arm's error body was previously unbounded as
            // well — pin the bound alongside the scrub.
            let hostile_compat = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/embeddings"))
                .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                    // Truncation applies to the whole "API error (401): "-
                    // prefixed message (~38 fixed chars before the filler),
                    // so the key spans ~478..522.
                    "error": { "message": format!("{}{COMPAT_KEY} rejected", "x".repeat(440)) }
                })))
                .mount(&hostile_compat)
                .await;
            let resp = h
                .api(Method::PUT, "alpha", "/api/account/provider")
                .bearer_auth(&token)
                .json(&json!({
                    "provider": "openai_compat",
                    "api_key": COMPAT_KEY,
                    "model_config": {
                        "embedding_model": "mock-embed",
                        "openai_compat_base_url": hostile_compat.uri(),
                    },
                }))
                .send()
                .await
                .expect("send byok put");
            let (status, body) = h.read(resp).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            let message = body["message"].as_str().expect("message");
            assert!(
                message.contains("[redacted]"),
                "compat echo is scrubbed: {message}"
            );
            for fragment in [
                COMPAT_KEY,
                &COMPAT_KEY[..12],
                &COMPAT_KEY[COMPAT_KEY.len() - 12..],
            ] {
                assert!(
                    !message.contains(fragment),
                    "no fragment of the key may survive on the compat arm: {message}"
                );
            }
            assert!(
                message.chars().count() <= 600,
                "the compat arm's provider error must be length-bounded, got {} chars",
                message.chars().count()
            );

            // Belt and braces: neither key in any body, nothing stored.
            h.assert_bodies_free_of(OR_KEY, "openrouter boundary key");
            h.assert_bodies_free_of(COMPAT_KEY, "compat boundary key");
            assert!(!control_db_contains(&url, OR_KEY).await);
            assert!(!control_db_contains(&url, COMPAT_KEY).await);

            h.stop().await;
        },
    )
    .await;
}

/// Rotation convergence, same pod (the rebuild-race shape): credentials are
/// rotated through the control-plane store directly — bumping the provider
/// generation but never touching this pod's cache, exactly the divergence a
/// build/rotate race leaves behind. One authenticated request later the
/// entry has refreshed in place (no rebuild), and the next embed uses the
/// NEW provider.
#[actix_web::test]
async fn out_of_band_rotation_heals_on_next_request() {
    with_control_db(
        "out_of_band_rotation_heals_on_next_request",
        |url| async move {
            let h = ProviderHarness::spawn_managed(&url).await;
            let (account, token) = h.provision("alpha").await;
            let account_id = &account.account_id;

            // Warm this pod's cache through the managed config.
            h.embed_atom("alpha", &token, "Pre-rotation note via managed.")
                .await;
            let handle_before = h.cache.get_or_load(account_id).await.expect("cached entry");

            // Out-of-band rotation: store writes only (these bump the
            // generation transactionally); the pod's cached config is now stale.
            let byok_mock = MockAiServer::start().await;
            let vault = support::test_vault();
            atomic_cloud::upsert_credentials(
                &h.control,
                vault.as_ref(),
                account_id,
                atomic_cloud::NewCredentials {
                    provider: atomic_cloud::Provider::OpenAiCompat,
                    origin: atomic_cloud::CredentialOrigin::User,
                    api_key: atomic_cloud::SecretKey::new("sk-byok-out-of-band".to_string()),
                    external_key_id: None,
                    model_config: json!({
                        "embedding_model": "mock-embed",
                        "llm_model": "mock-llm",
                        "openai_compat_base_url": byok_mock.base_url(),
                        "embedding_dimension": 1536,
                    }),
                },
            )
            .await
            .expect("rotate credentials out of band");
            atomic_cloud::set_active_provider(
                &h.control,
                account_id,
                Some((
                    atomic_cloud::Provider::OpenAiCompat,
                    atomic_cloud::CredentialOrigin::User,
                )),
            )
            .await
            .expect("flip active pointer out of band");

            // One authenticated request — any request — heals the divergence.
            let resp = h
                .api(Method::GET, "alpha", "/api/account/provider")
                .bearer_auth(&token)
                .send()
                .await
                .expect("send status");
            let (status, _body) = h.read(resp).await;
            assert_eq!(status, StatusCode::OK);

            // The next embed uses the NEW provider; the managed endpoint sees
            // nothing further.
            let managed_before = h.managed_mock.embedding_request_count();
            h.embed_atom("alpha", &token, "Post-rotation note via BYOK.")
                .await;
            assert!(
                byok_mock.embedding_request_count() >= 1,
                "post-heal embeds must hit the rotated provider"
            );
            assert_eq!(
                h.managed_mock.embedding_request_count(),
                managed_before,
                "the stale managed config must not serve after the heal"
            );

            // The heal was an in-place refresh, not an eviction.
            let handle_after = h.cache.get_or_load(account_id).await.expect("cached entry");
            assert!(
                Arc::ptr_eq(&handle_before.manager, &handle_after.manager),
                "generation refresh must swap the config in place"
            );

            h.stop().await;
        },
    )
    .await;
}

/// Rotation convergence, cross-pod: a second AccountCache over the same
/// control plane (a second serve process) holds the pre-rotation config; a
/// BYOK save through the FIRST pod's HTTP route rotates storage and bumps
/// the generation. The second pod's next generation-checked resolution —
/// what its CloudAuth performs on every request — picks up the new config
/// without any cross-pod signalling.
#[actix_web::test]
async fn second_pod_sees_rotation_after_one_request() {
    with_control_db(
        "second_pod_sees_rotation_after_one_request",
        |url| async move {
            let h = ProviderHarness::spawn_managed(&url).await;
            let (account, token) = h.provision("alpha").await;
            let account_id = &account.account_id;

            // Pod 2, warmed before the rotation: serves the managed config.
            let pod2 = AccountCache::new(
                h.control.clone(),
                h.cluster.clone(),
                support::test_vault(),
                AccountCacheConfig::default(),
            );
            let handle2 = pod2.get_or_load(account_id).await.expect("pod-2 entry");
            let core2 = handle2.manager.active_core().await.expect("pod-2 core");
            let before = core2
                .active_provider_config()
                .expect("cloud cores always run explicit configs");
            assert_eq!(before.provider_type, atomic_core::ProviderType::OpenRouter);

            // Rotation through pod 1's HTTP route (validated against the BYOK
            // mock, stored, generation bumped, pod 1 live-swapped).
            let byok_mock = MockAiServer::start().await;
            let resp = h
                .api(Method::PUT, "alpha", "/api/account/provider")
                .bearer_auth(&token)
                .json(&json!({
                    "provider": "openai_compat",
                    "api_key": "sk-byok-cross-pod",
                    "model_config": {
                        "embedding_model": "mock-embed",
                        "llm_model": "mock-llm",
                        "openai_compat_base_url": byok_mock.base_url(),
                        "embedding_dimension": 1536,
                    },
                }))
                .send()
                .await
                .expect("send byok put");
            let (status, body) = h.read(resp).await;
            assert_eq!(status, StatusCode::OK, "{body}");

            // Pod 2's next request: its auth layer reads the bumped generation
            // and resolves through the generation-checked path — the stale
            // entry refreshes in place.
            let generation: i64 =
                sqlx::query_scalar("SELECT provider_generation FROM accounts WHERE id = $1")
                    .bind(account_id)
                    .fetch_one(h.control.pool())
                    .await
                    .expect("read generation");
            let handle2_after = pod2
                .get_or_load_with_generation(account_id, generation)
                .await
                .expect("pod-2 generation-checked resolve");
            assert!(
                Arc::ptr_eq(&handle2.manager, &handle2_after.manager),
                "pod 2 refreshes in place, no rebuild"
            );
            let after = core2
                .active_provider_config()
                .expect("explicit config still active");
            assert_eq!(
                after.provider_type,
                atomic_core::ProviderType::OpenAICompat,
                "pod 2 must serve the rotated provider after one request"
            );
            assert_eq!(after.openai_compat_base_url, byok_mock.base_url());

            h.stop().await;
        },
    )
    .await;
}
