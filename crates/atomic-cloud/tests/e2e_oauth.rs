//! End-to-end tests for cloud's per-account OAuth flow + per-tenant MCP
//! (plan: "Auth & tenant routing" → "OAuth", "MCP token UX"; slice 7).
//!
//! Each test spawns the real composition — `configure_cloud_app` on an
//! ephemeral port, exactly as `atomic-cloud serve` wires it — provisions
//! accounts against the test cluster, and drives them with `reqwest` over a
//! loopback listener with an explicit `Host` header
//! (`alpha.cloudtest.local`). The flow is the standard discovery → DCR →
//! Authorization Code + PKCE → token exchange, with the approve step
//! authenticated by the session cookie (not a pasted token).
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command.

mod support;

use std::sync::Arc;
use std::time::Duration;

use actix_web::{App, HttpServer};
use atomic_cloud::{
    configure_cloud_app, create_session, insert_oauth_code, issue_token, provision_account,
    set_active_provider, upsert_credentials, AccountCache, AccountCacheConfig, AccountPlane,
    AccountPlaneConfig, ChatStreamLimiter, CloudAuth, ClusterConfig, ControlPlane,
    CredentialOrigin, FallbackAppState, ManagedKeys, NewAccount, NewCredentials, NewOAuthCode,
    OAuthPlane, Provider, QuotaBilling, Readiness, SecretKey, TenantPlane, TokenScope,
    DEFAULT_CHAT_STREAMS_PER_ACCOUNT, SESSION_COOKIE,
};
use atomic_test_support::MockAiServer;
use reqwest::header::{HOST, LOCATION, WWW_AUTHENTICATE};
use reqwest::{redirect::Policy, Method, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use support::with_control_db;

const BASE_DOMAIN: &str = "cloudtest.local";

/// RFC 7636 Appendix B canonical PKCE pair — pinned so the S256 verification
/// path is exercised against a known-good fixture.
const RFC7636_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const RFC7636_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

const REDIRECT_URI: &str = "https://claude.ai/api/mcp/auth_callback";

struct Account {
    account_id: String,
    /// An account-scope token minted at provision time, used to seed the
    /// tenant's KB over `/api/*` so the MCP `tools/call` has something real to
    /// read back (the OAuth-minted token is exercised separately).
    account_token: String,
}

struct OAuthHarness {
    control: ControlPlane,
    cluster: ClusterConfig,
    mock: MockAiServer,
    /// A non-redirect-following client: the OAuth flow's 302s carry the code,
    /// which the test inspects rather than chases.
    client: reqwest::Client,
    base_url: String,
    handle: actix_web::dev::ServerHandle,
    _fallback: FallbackAppState,
}

impl OAuthHarness {
    async fn spawn(control_url: &str) -> Self {
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
            support::test_vault(),
            AccountCacheConfig::default(),
        ));
        // `http` scheme like a local/dev deploy, so the MCP `WWW-Authenticate`
        // challenge points at the same `http://<sub>.<base>` origin the OAuth
        // discovery is served from (the harness drives plain HTTP on loopback).
        let auth = CloudAuth::new(control.clone(), Arc::clone(&cache), BASE_DOMAIN)
            .with_public_scheme("http");
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
        let oauth_plane = OAuthPlane::new(
            control.clone(),
            BASE_DOMAIN,
            "http",
            format!("http://app.{BASE_DOMAIN}"),
        );
        let mcp_transport = fallback.mcp_transport(atomic_cloud::DEFAULT_MCP_SSE_KEEP_ALIVE);
        let control_for_app = control.clone();
        let chat_streams = ChatStreamLimiter::new(DEFAULT_CHAT_STREAMS_PER_ACCOUNT);
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

        OAuthHarness {
            control,
            cluster,
            mock,
            client: reqwest::Client::builder()
                .redirect(Policy::none())
                .build()
                .expect("client"),
            base_url: format!("http://127.0.0.1:{port}"),
            handle,
            _fallback: fallback,
        }
    }

    async fn stop(self) {
        self.handle.stop(false).await;
    }

    /// Provision an account with mock provider credentials so its tenant
    /// loads (the cache resolves provider config from the control plane).
    async fn provision(&self, subdomain: &str) -> Account {
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

        // An account-scope token to seed atoms over /api/* (the data plane the
        // OAuth-minted MCP token later reads).
        let account_token = issue_token(
            &self.control,
            &account.account_id,
            TokenScope::Account,
            None,
            "test: seed",
        )
        .await
        .expect("issue account token");

        Account {
            account_id: account.account_id,
            account_token,
        }
    }

    /// Seed an atom on `subdomain`'s default KB via the account token and
    /// return its id. Atom creation is synchronous from the caller's
    /// perspective (the embedding pipeline runs in the background), so the id
    /// is readable immediately — no provider round-trip is needed for a
    /// subsequent `read_atom`.
    async fn seed_atom(&self, subdomain: &str, token: &str, content: &str) -> String {
        let resp = self
            .req(Method::POST, subdomain, "/api/atoms")
            .bearer_auth(token)
            .json(&json!({ "content": content }))
            .send()
            .await
            .expect("send create atom");
        assert_eq!(resp.status(), StatusCode::CREATED, "seed atom");
        let body: Value = resp.json().await.expect("atom json");
        body["id"]
            .as_str()
            .or_else(|| body["atom"]["id"].as_str())
            .expect("atom id in create response")
            .to_string()
    }

    /// Create a second knowledge base on `subdomain` over the account token,
    /// returning its `db_id`. Used to give a tenant two KBs so a db-pinned MCP
    /// token can be exercised against the non-active one.
    async fn create_kb(&self, subdomain: &str, token: &str, name: &str) -> String {
        let resp = self
            .req(Method::POST, subdomain, "/api/databases")
            .bearer_auth(token)
            .json(&json!({ "name": name }))
            .send()
            .await
            .expect("send create database");
        assert_eq!(resp.status(), StatusCode::CREATED, "create KB");
        let body: Value = resp.json().await.expect("database json");
        body["id"].as_str().expect("db id").to_string()
    }

    /// Seed an atom into a SPECIFIC KB (`db_id`) on `subdomain`, selecting the
    /// KB with the `X-Atomic-Database` header exactly as the data plane does.
    /// Returns the new atom's id.
    async fn seed_atom_in_db(
        &self,
        subdomain: &str,
        token: &str,
        db_id: &str,
        content: &str,
    ) -> String {
        let resp = self
            .req(Method::POST, subdomain, "/api/atoms")
            .bearer_auth(token)
            .header("X-Atomic-Database", db_id)
            .json(&json!({ "content": content }))
            .send()
            .await
            .expect("send create atom in db");
        assert_eq!(resp.status(), StatusCode::CREATED, "seed atom in db");
        let body: Value = resp.json().await.expect("atom json");
        body["id"]
            .as_str()
            .or_else(|| body["atom"]["id"].as_str())
            .expect("atom id in create response")
            .to_string()
    }

    /// Drive the MCP Streamable-HTTP handshake against `subdomain`'s `/mcp`
    /// with `token`, then issue a single `tools/call`. Returns the tool
    /// result's first text content.
    ///
    /// rmcp's transport is stateful: `initialize` mints a session id (the
    /// `mcp-session-id` response header), the client confirms with a
    /// `notifications/initialized`, and only then are `tools/call`s accepted —
    /// the same handshake the transport's own unit tests drive.
    async fn mcp_tools_call(
        &self,
        subdomain: &str,
        token: &str,
        tool: &str,
        arguments: Value,
    ) -> String {
        let init = self.mcp_initialize(subdomain, token).await;
        assert_eq!(init.status(), StatusCode::OK, "mcp initialize");
        let session_id = init
            .headers()
            .get("mcp-session-id")
            .expect("initialize establishes a session")
            .to_str()
            .expect("session id str")
            .to_string();

        // Confirm the session before calling tools.
        let initialized = self
            .req(Method::POST, subdomain, "/mcp")
            .bearer_auth(token)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .header("mcp-session-id", &session_id)
            .body(
                json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                })
                .to_string(),
            )
            .send()
            .await
            .expect("send initialized");
        assert!(
            initialized.status().is_success(),
            "initialized notification accepted: {}",
            initialized.status()
        );

        let resp = self
            .req(Method::POST, subdomain, "/mcp")
            .bearer_auth(token)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .header("mcp-session-id", &session_id)
            .body(
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/call",
                    "params": { "name": tool, "arguments": arguments }
                })
                .to_string(),
            )
            .send()
            .await
            .expect("send tools/call");
        assert_eq!(resp.status(), StatusCode::OK, "tools/call");
        let body = resp.text().await.expect("tools/call body");
        tool_result_text(&body)
    }

    fn req(&self, method: Method, subdomain: &str, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base_url))
            .header(HOST, format!("{subdomain}.{BASE_DOMAIN}"))
    }

    /// Register an OAuth client (DCR) for `subdomain`, returning
    /// `(client_id, client_secret)`.
    async fn register_client(&self, subdomain: &str) -> (String, String) {
        let resp = self
            .req(Method::POST, subdomain, "/oauth/register")
            .json(&json!({
                "client_name": "Claude Desktop",
                "redirect_uris": [REDIRECT_URI],
            }))
            .send()
            .await
            .expect("send register");
        assert_eq!(resp.status(), StatusCode::CREATED, "DCR succeeds");
        let body: Value = resp.json().await.expect("register json");
        (
            body["client_id"].as_str().expect("client_id").to_string(),
            body["client_secret"]
                .as_str()
                .expect("client_secret")
                .to_string(),
        )
    }

    /// Mint a session cookie for `account_id` (the logged-in browser the
    /// approve step authenticates).
    async fn session(&self, account_id: &str) -> String {
        create_session(
            &self.control,
            account_id,
            Duration::from_secs(3600),
            None,
            None,
        )
        .await
        .expect("create session")
    }

    /// POST the approve form with a session cookie and return the redirect
    /// `Location` (the `code` lives in its query string).
    async fn approve(
        &self,
        subdomain: &str,
        session: &str,
        client_id: &str,
        challenge: &str,
        state: &str,
    ) -> String {
        let resp = self
            .req(Method::POST, subdomain, "/oauth/authorize")
            .header("Cookie", format!("{SESSION_COOKIE}={session}"))
            .form(&[
                ("client_id", client_id),
                ("redirect_uri", REDIRECT_URI),
                ("code_challenge", challenge),
                ("code_challenge_method", "S256"),
                ("state", state),
                ("action", "approve"),
            ])
            .send()
            .await
            .expect("send approve");
        assert_eq!(resp.status(), StatusCode::FOUND, "approve redirects");
        resp.headers()
            .get(LOCATION)
            .expect("Location")
            .to_str()
            .expect("location str")
            .to_string()
    }

    /// POST an MCP `initialize` to `subdomain`'s `/mcp` with the given bearer
    /// token (the OAuth-minted mcp token), returning the raw response.
    async fn mcp_initialize(&self, subdomain: &str, token: &str) -> reqwest::Response {
        self.req(Method::POST, subdomain, "/mcp")
            .bearer_auth(token)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .body(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "clientInfo": { "name": "claude", "version": "0" }
                    }
                })
                .to_string(),
            )
            .send()
            .await
            .expect("send mcp initialize")
    }

    /// Exchange a code at the token endpoint, returning the raw response.
    async fn token(
        &self,
        subdomain: &str,
        client_id: &str,
        client_secret: &str,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> reqwest::Response {
        self.req(Method::POST, subdomain, "/oauth/token")
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("client_id", client_id),
                ("client_secret", client_secret),
                ("code_verifier", verifier),
                ("redirect_uri", redirect_uri),
            ])
            .send()
            .await
            .expect("send token")
    }
}

/// Pull the first text content out of an MCP `tools/call` response. The
/// Streamable-HTTP transport frames the JSON-RPC reply as an SSE `data:` line;
/// dig out `result.content[0].text`.
fn tool_result_text(body: &str) -> String {
    let payload = body
        .lines()
        .find_map(|l| l.strip_prefix("data: "))
        .unwrap_or(body);
    let value: Value = serde_json::from_str(payload)
        .unwrap_or_else(|e| panic!("tools/call body is not JSON-RPC: {e}\nbody = {body}"));
    value["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text content in tools/call result: {value}"))
        .to_string()
}

/// Extract the `code` query parameter from a redirect Location.
fn code_from_location(location: &str) -> String {
    let url = reqwest::Url::parse(location).expect("parse location");
    url.query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.to_string())
        .unwrap_or_else(|| panic!("no code in {location}"))
}

#[actix_web::test]
async fn full_flow_session_to_mcp_scoped_token_that_cloudauth_accepts() {
    with_control_db(
        "full_flow_session_to_mcp_scoped_token_that_cloudauth_accepts",
        |url| async move {
            let h = OAuthHarness::spawn(&url).await;
            let account = h.provision("alpha").await;

            // Discovery points at the tenant's own host.
            let meta: Value = h
                .req(
                    Method::GET,
                    "alpha",
                    "/.well-known/oauth-authorization-server",
                )
                .send()
                .await
                .expect("send discovery")
                .json()
                .await
                .expect("discovery json");
            assert_eq!(meta["issuer"], "http://alpha.cloudtest.local");
            assert_eq!(
                meta["authorization_endpoint"],
                "http://alpha.cloudtest.local/oauth/authorize"
            );
            assert_eq!(meta["code_challenge_methods_supported"][0], "S256");
            let res: Value = h
                .req(
                    Method::GET,
                    "alpha",
                    "/.well-known/oauth-protected-resource/mcp",
                )
                .send()
                .await
                .expect("send pr discovery")
                .json()
                .await
                .expect("pr json");
            assert_eq!(res["resource"], "http://alpha.cloudtest.local/mcp");

            // DCR.
            let (client_id, client_secret) = h.register_client("alpha").await;

            // Authorize (GET) with a session renders the consent page.
            let session = h.session(&account.account_id).await;
            let consent = h
                .req(
                    Method::GET,
                    "alpha",
                    &format!(
                        "/oauth/authorize?client_id={client_id}\
                         &redirect_uri={REDIRECT_URI}&response_type=code\
                         &code_challenge={RFC7636_CHALLENGE}&code_challenge_method=S256&state=xyz"
                    ),
                )
                .header("Cookie", format!("{SESSION_COOKIE}={session}"))
                .send()
                .await
                .expect("send authorize get");
            assert_eq!(consent.status(), StatusCode::OK, "consent page renders");
            // The consent page lives on the tenant origin and its approval POST
            // rides the SameSite=Lax session cookie, so it MUST deny all
            // framing — otherwise an attacker who completed their own DCR could
            // clickjack the logged-in user into minting them a token. Both the
            // legacy X-Frame-Options and the modern CSP frame-ancestors are
            // asserted (the consent GET is the only OAuth HTML response).
            assert_eq!(
                consent
                    .headers()
                    .get("X-Frame-Options")
                    .and_then(|v| v.to_str().ok()),
                Some("DENY"),
                "consent page denies framing (X-Frame-Options)"
            );
            assert_eq!(
                consent
                    .headers()
                    .get("Content-Security-Policy")
                    .and_then(|v| v.to_str().ok()),
                Some("frame-ancestors 'none'"),
                "consent page denies framing (CSP frame-ancestors)"
            );
            let consent_body = consent.text().await.expect("consent body");
            assert!(consent_body.contains("Approve"), "consent has approve");
            assert!(
                consent_body.contains("Claude Desktop"),
                "consent names the client"
            );

            // Approve → code.
            let location = h
                .approve("alpha", &session, &client_id, RFC7636_CHALLENGE, "xyz")
                .await;
            assert!(
                location.starts_with(REDIRECT_URI),
                "redirect to the registered uri: {location}"
            );
            assert!(location.contains("state=xyz"), "state echoed");
            let code = code_from_location(&location);

            // Token exchange with the correct verifier → Bearer token.
            let resp = h
                .token(
                    "alpha",
                    &client_id,
                    &client_secret,
                    &code,
                    RFC7636_VERIFIER,
                    REDIRECT_URI,
                )
                .await;
            assert_eq!(resp.status(), StatusCode::OK, "token issued");
            let body: Value = resp.json().await.expect("token json");
            assert_eq!(body["token_type"], "Bearer");
            let access_token = body["access_token"].as_str().expect("access_token");
            assert!(access_token.starts_with("atm_"), "cloud token shape");

            // The minted row is mcp-scoped, account-scope (no db pin) — the
            // slice's default. Verified directly in the control plane.
            let (scope, allowed_db): (String, Option<String>) =
                sqlx::query_as("SELECT scope, allowed_db_id FROM cloud_tokens WHERE hash = $1")
                    .bind(data_encoding::HEXLOWER.encode(&Sha256::digest(access_token.as_bytes())))
                    .fetch_one(h.control.pool())
                    .await
                    .expect("token row");
            assert_eq!(scope, "mcp", "token is mcp-scoped");
            assert!(allowed_db.is_none(), "account-scope default: no db pin");

            // CloudAuth accepts it on the tenant's /api/* (the whole point).
            let api = h
                .req(Method::GET, "alpha", "/api/atoms")
                .bearer_auth(access_token)
                .send()
                .await
                .expect("send api");
            assert_eq!(
                api.status(),
                StatusCode::OK,
                "mcp token reaches the data plane"
            );

            // And it initializes an MCP session on the tenant's /mcp endpoint.
            let mcp = h
                .req(Method::POST, "alpha", "/mcp")
                .bearer_auth(access_token)
                .header("Accept", "application/json, text/event-stream")
                .header("Content-Type", "application/json")
                .body(
                    json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "initialize",
                        "params": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {},
                            "clientInfo": { "name": "claude", "version": "0" }
                        }
                    })
                    .to_string(),
                )
                .send()
                .await
                .expect("send mcp initialize");
            assert_eq!(
                mcp.status(),
                StatusCode::OK,
                "mcp initialize via the oauth token"
            );
            assert!(
                mcp.headers().contains_key("mcp-session-id"),
                "mcp session established"
            );

            h.stop().await;
        },
    )
    .await;
}

#[actix_web::test]
async fn approve_appends_code_with_ampersand_when_redirect_uri_has_query() {
    // RFC 6749 §3.1.2/§4.1.2: a registered redirect_uri MAY carry its own
    // query string; the authorization response must then append `code`/`state`
    // with `&`, not a second `?` (which would corrupt the redirect).
    with_control_db(
        "approve_appends_code_with_ampersand_when_redirect_uri_has_query",
        |url| async move {
            let h = OAuthHarness::spawn(&url).await;
            let account = h.provision("alpha").await;

            // A redirect_uri that already has a `?tenant=acme` query.
            let redirect_uri = "https://claude.ai/api/mcp/auth_callback?tenant=acme";

            let reg = h
                .req(Method::POST, "alpha", "/oauth/register")
                .json(&json!({
                    "client_name": "Claude Desktop",
                    "redirect_uris": [redirect_uri],
                }))
                .send()
                .await
                .expect("send register");
            assert_eq!(reg.status(), StatusCode::CREATED, "DCR succeeds");
            let reg_body: Value = reg.json().await.expect("register json");
            let client_id = reg_body["client_id"].as_str().expect("client_id");

            let session = h.session(&account.account_id).await;
            let resp = h
                .req(Method::POST, "alpha", "/oauth/authorize")
                .header("Cookie", format!("{SESSION_COOKIE}={session}"))
                .form(&[
                    ("client_id", client_id),
                    ("redirect_uri", redirect_uri),
                    ("code_challenge", RFC7636_CHALLENGE),
                    ("code_challenge_method", "S256"),
                    ("state", "xyz"),
                    ("action", "approve"),
                ])
                .send()
                .await
                .expect("send approve");
            assert_eq!(resp.status(), StatusCode::FOUND, "approve redirects");
            let location = resp
                .headers()
                .get(LOCATION)
                .expect("Location")
                .to_str()
                .expect("location str")
                .to_string();

            // The existing `?tenant=acme` is preserved, and code/state are
            // appended with `&` — never a second `?`.
            assert!(
                location.starts_with("https://claude.ai/api/mcp/auth_callback?tenant=acme"),
                "existing query preserved: {location}"
            );
            assert!(
                location.contains("&code="),
                "code appended with `&`: {location}"
            );
            assert!(
                location.contains("&state=xyz"),
                "state appended with `&`: {location}"
            );
            assert_eq!(
                location.matches('?').count(),
                1,
                "exactly one `?` in the redirect: {location}"
            );

            // And the code is still a valid, exchangeable grant (the `&`
            // separator didn't mangle it).
            let code = code_from_location(&location);
            assert!(!code.is_empty(), "code parsed from the redirect");

            h.stop().await;
        },
    )
    .await;
}

#[actix_web::test]
async fn oauth_token_tools_call_operates_on_owning_tenant_kb() {
    // The full Claude-Desktop journey, carried through to a real `tools/call`:
    // the OAuth-minted token reads back an atom that lives only in alpha's KB,
    // and the SAME tool call cannot see beta's atom — proving the token both
    // operates on its owning tenant's data AND is tenant-isolated at the data
    // layer, not merely at the auth boundary.
    with_control_db(
        "oauth_token_tools_call_operates_on_owning_tenant_kb",
        |url| async move {
            let h = OAuthHarness::spawn(&url).await;
            let alpha = h.provision("alpha").await;
            let bravo = h.provision("bravo").await;

            // Seed one atom in each tenant's KB over the account API.
            let alpha_atom = h
                .seed_atom(
                    "alpha",
                    &alpha.account_token,
                    "ALPHA-SECRET: the rust workspace layout",
                )
                .await;
            let bravo_atom = h
                .seed_atom(
                    "bravo",
                    &bravo.account_token,
                    "BRAVO-SECRET: the postgres tenant schema",
                )
                .await;

            // Mint an MCP token for alpha through the real OAuth flow.
            let (client_id, client_secret) = h.register_client("alpha").await;
            let session = h.session(&alpha.account_id).await;
            let location = h
                .approve("alpha", &session, &client_id, RFC7636_CHALLENGE, "s")
                .await;
            let code = code_from_location(&location);
            let resp = h
                .token(
                    "alpha",
                    &client_id,
                    &client_secret,
                    &code,
                    RFC7636_VERIFIER,
                    REDIRECT_URI,
                )
                .await;
            assert_eq!(resp.status(), StatusCode::OK, "token issued");
            let token = resp.json::<Value>().await.expect("token json")["access_token"]
                .as_str()
                .expect("access_token")
                .to_string();

            // read_atom on alpha's atom via MCP returns its body: the OAuth
            // token operates on alpha's own knowledge base (CloudAuth injects
            // alpha's manager; the transport resolves it per-request).
            let text = h
                .mcp_tools_call(
                    "alpha",
                    &token,
                    "read_atom",
                    json!({ "atom_id": alpha_atom }),
                )
                .await;
            assert!(
                text.contains("ALPHA-SECRET"),
                "read_atom returns alpha's atom body via the oauth token: {text}"
            );

            // The same tool call for BRAVO's atom id — still on alpha's /mcp,
            // alpha's token — cannot see it: bravo's atom lives in bravo's
            // tenant DB, which alpha's resolved manager never touches.
            let text = h
                .mcp_tools_call(
                    "alpha",
                    &token,
                    "read_atom",
                    json!({ "atom_id": bravo_atom }),
                )
                .await;
            assert!(
                text.contains("Atom not found"),
                "alpha's mcp token cannot read bravo's atom across tenants: {text}"
            );

            h.stop().await;
        },
    )
    .await;
}

#[actix_web::test]
async fn wrong_pkce_verifier_is_rejected() {
    with_control_db("wrong_pkce_verifier_is_rejected", |url| async move {
        let h = OAuthHarness::spawn(&url).await;
        let account = h.provision("alpha").await;
        let (client_id, client_secret) = h.register_client("alpha").await;
        let session = h.session(&account.account_id).await;

        let location = h
            .approve("alpha", &session, &client_id, RFC7636_CHALLENGE, "s")
            .await;
        let code = code_from_location(&location);

        // A verifier that doesn't hash to the challenge → invalid_grant.
        let resp = h
            .token(
                "alpha",
                &client_id,
                &client_secret,
                &code,
                "the-wrong-verifier",
                REDIRECT_URI,
            )
            .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body: Value = resp.json().await.expect("err json");
        assert_eq!(body["error"], "invalid_grant");

        h.stop().await;
    })
    .await;
}

#[actix_web::test]
async fn authorization_code_is_single_use() {
    with_control_db("authorization_code_is_single_use", |url| async move {
        let h = OAuthHarness::spawn(&url).await;
        let account = h.provision("alpha").await;
        let (client_id, client_secret) = h.register_client("alpha").await;
        let session = h.session(&account.account_id).await;

        let location = h
            .approve("alpha", &session, &client_id, RFC7636_CHALLENGE, "s")
            .await;
        let code = code_from_location(&location);

        // First exchange wins.
        let first = h
            .token(
                "alpha",
                &client_id,
                &client_secret,
                &code,
                RFC7636_VERIFIER,
                REDIRECT_URI,
            )
            .await;
        assert_eq!(first.status(), StatusCode::OK, "first exchange succeeds");

        // Replay of the same code mints no second token.
        let replay = h
            .token(
                "alpha",
                &client_id,
                &client_secret,
                &code,
                RFC7636_VERIFIER,
                REDIRECT_URI,
            )
            .await;
        assert_eq!(replay.status(), StatusCode::BAD_REQUEST, "replay rejected");
        let body: Value = replay.json().await.expect("err json");
        assert_eq!(body["error"], "invalid_grant");

        // Exactly one mcp token exists for the account.
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM cloud_tokens WHERE account_id = $1 AND scope = 'mcp'",
        )
        .bind(&account.account_id)
        .fetch_one(h.control.pool())
        .await
        .expect("count tokens");
        assert_eq!(count, 1, "the replay minted no second token");

        h.stop().await;
    })
    .await;
}

#[actix_web::test]
async fn redirect_uri_mismatch_rejected_at_authorize_and_token() {
    with_control_db(
        "redirect_uri_mismatch_rejected_at_authorize_and_token",
        |url| async move {
            let h = OAuthHarness::spawn(&url).await;
            let account = h.provision("alpha").await;
            let (client_id, client_secret) = h.register_client("alpha").await;
            let session = h.session(&account.account_id).await;

            // At authorize: an unregistered redirect_uri is a 400 (we can't
            // safely redirect errors to a URI we don't trust).
            let resp = h
                .req(Method::POST, "alpha", "/oauth/authorize")
                .header("Cookie", format!("{SESSION_COOKIE}={session}"))
                .form(&[
                    ("client_id", client_id.as_str()),
                    ("redirect_uri", "https://evil.example/callback"),
                    ("code_challenge", RFC7636_CHALLENGE),
                    ("code_challenge_method", "S256"),
                    ("state", "s"),
                    ("action", "approve"),
                ])
                .send()
                .await
                .expect("send approve");
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "authorize rejects bad uri"
            );

            // At token: mint a legit code, then present a different
            // redirect_uri at exchange → invalid_grant.
            let location = h
                .approve("alpha", &session, &client_id, RFC7636_CHALLENGE, "s")
                .await;
            let code = code_from_location(&location);
            let resp = h
                .token(
                    "alpha",
                    &client_id,
                    &client_secret,
                    &code,
                    RFC7636_VERIFIER,
                    "https://evil.example/callback",
                )
                .await;
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "token rejects mismatch"
            );
            let body: Value = resp.json().await.expect("err json");
            assert_eq!(body["error"], "invalid_grant");

            h.stop().await;
        },
    )
    .await;
}

#[actix_web::test]
async fn expired_code_is_rejected_at_token() {
    with_control_db("expired_code_is_rejected_at_token", |url| async move {
        let h = OAuthHarness::spawn(&url).await;
        let account = h.provision("alpha").await;
        let (client_id, client_secret) = h.register_client("alpha").await;

        // Insert a born-expired code directly (zero TTL) — the approve route
        // always issues a live one, so this drives the expiry branch of the
        // token endpoint without sleeping out the real TTL.
        let code = insert_oauth_code(
            &h.control,
            NewOAuthCode {
                account_id: &account.account_id,
                client_id: &client_id,
                code_challenge: RFC7636_CHALLENGE,
                code_challenge_method: "S256",
                redirect_uri: REDIRECT_URI,
                scope: TokenScope::Mcp,
                allowed_db_id: None,
            },
            Duration::from_secs(0),
        )
        .await
        .expect("insert expired code");

        let resp = h
            .token(
                "alpha",
                &client_id,
                &client_secret,
                &code,
                RFC7636_VERIFIER,
                REDIRECT_URI,
            )
            .await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "expired code rejected"
        );
        let body: Value = resp.json().await.expect("err json");
        assert_eq!(body["error"], "invalid_grant");

        h.stop().await;
    })
    .await;
}

#[actix_web::test]
async fn authorize_without_session_redirects_to_login_and_mints_no_code() {
    with_control_db(
        "authorize_without_session_redirects_to_login_and_mints_no_code",
        |url| async move {
            let h = OAuthHarness::spawn(&url).await;
            let account = h.provision("alpha").await;
            let (client_id, _secret) = h.register_client("alpha").await;

            // GET authorize with NO session cookie → bounce to login.
            let resp = h
                .req(
                    Method::GET,
                    "alpha",
                    &format!(
                        "/oauth/authorize?client_id={client_id}\
                         &redirect_uri={REDIRECT_URI}&response_type=code\
                         &code_challenge={RFC7636_CHALLENGE}&code_challenge_method=S256&state=z"
                    ),
                )
                .send()
                .await
                .expect("send authorize get");
            assert_eq!(
                resp.status(),
                StatusCode::FOUND,
                "redirects when logged out"
            );
            let location = resp
                .headers()
                .get(LOCATION)
                .expect("Location")
                .to_str()
                .expect("loc str");
            assert!(
                location.starts_with("http://app.cloudtest.local/login?return_to="),
                "bounces to the app-host login with return_to: {location}"
            );

            // POST approve with no session also mints no code (it bounces).
            let resp = h
                .req(Method::POST, "alpha", "/oauth/authorize")
                .form(&[
                    ("client_id", client_id.as_str()),
                    ("redirect_uri", REDIRECT_URI),
                    ("code_challenge", RFC7636_CHALLENGE),
                    ("code_challenge_method", "S256"),
                    ("state", "z"),
                    ("action", "approve"),
                ])
                .send()
                .await
                .expect("send approve no session");
            assert_eq!(
                resp.status(),
                StatusCode::FOUND,
                "logged-out approve bounces"
            );
            let location = resp
                .headers()
                .get(LOCATION)
                .expect("Location")
                .to_str()
                .expect("loc str");
            assert!(location.contains("/login"), "approve bounces to login too");

            // No code was ever minted for this account.
            let codes: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM oauth_codes WHERE account_id = $1")
                    .bind(&account.account_id)
                    .fetch_one(h.control.pool())
                    .await
                    .expect("count codes");
            assert_eq!(codes, 0, "no authorization code minted without a session");

            h.stop().await;
        },
    )
    .await;
}

#[actix_web::test]
async fn cross_tenant_client_id_is_invalid() {
    with_control_db("cross_tenant_client_id_is_invalid", |url| async move {
        let h = OAuthHarness::spawn(&url).await;
        let alpha = h.provision("alpha").await;
        let bravo = h.provision("bravo").await;

        // A client registered under alpha...
        let (alpha_client, alpha_secret) = h.register_client("alpha").await;
        let beta_session = h.session(&bravo.account_id).await;

        // ...presented on bravo's subdomain at authorize → invalid_client
        // (bravo can't resolve alpha's client_id; the cross-tenant chokepoint).
        let resp = h
            .req(
                Method::GET,
                "bravo",
                &format!(
                    "/oauth/authorize?client_id={alpha_client}\
                     &redirect_uri={REDIRECT_URI}&response_type=code\
                     &code_challenge={RFC7636_CHALLENGE}&code_challenge_method=S256&state=s"
                ),
            )
            .header("Cookie", format!("{SESSION_COOKIE}={beta_session}"))
            .send()
            .await
            .expect("send authorize");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body: Value = resp.json().await.expect("err json");
        assert_eq!(body["error"], "invalid_client");

        // And at the token endpoint: mint a code under alpha properly...
        let alpha_session = h.session(&alpha.account_id).await;
        let location = h
            .approve(
                "alpha",
                &alpha_session,
                &alpha_client,
                RFC7636_CHALLENGE,
                "s",
            )
            .await;
        let code = code_from_location(&location);
        // ...then try to redeem alpha's client+code on bravo's subdomain →
        // invalid_client (bravo can't resolve alpha's client).
        let resp = h
            .token(
                "bravo",
                &alpha_client,
                &alpha_secret,
                &code,
                RFC7636_VERIFIER,
                REDIRECT_URI,
            )
            .await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "cross-tenant token"
        );
        let body: Value = resp.json().await.expect("err json");
        assert_eq!(body["error"], "invalid_client");

        h.stop().await;
    })
    .await;
}

#[actix_web::test]
async fn bad_client_secret_is_unauthorized() {
    with_control_db("bad_client_secret_is_unauthorized", |url| async move {
        let h = OAuthHarness::spawn(&url).await;
        let account = h.provision("alpha").await;
        let (client_id, _secret) = h.register_client("alpha").await;
        let session = h.session(&account.account_id).await;

        let location = h
            .approve("alpha", &session, &client_id, RFC7636_CHALLENGE, "s")
            .await;
        let code = code_from_location(&location);

        let resp = h
            .token(
                "alpha",
                &client_id,
                "totally-wrong-secret",
                &code,
                RFC7636_VERIFIER,
                REDIRECT_URI,
            )
            .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body: Value = resp.json().await.expect("err json");
        assert_eq!(body["error"], "invalid_client");

        h.stop().await;
    })
    .await;
}

#[actix_web::test]
async fn discovery_and_register_404_on_app_host() {
    with_control_db("discovery_and_register_404_on_app_host", |url| async move {
        let h = OAuthHarness::spawn(&url).await;
        let _account = h.provision("alpha").await;

        // The app host resolves no subdomain → not_found, exactly like a
        // tenant route. Bootstrap endpoints are tenant-only.
        for host in ["cloudtest.local", "app.cloudtest.local"] {
            let resp = h
                .client
                .get(format!(
                    "{}/.well-known/oauth-authorization-server",
                    h.base_url
                ))
                .header(HOST, host)
                .send()
                .await
                .expect("send discovery");
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "discovery is tenant-only ({host})"
            );

            let resp = h
                .client
                .post(format!("{}/oauth/register", h.base_url))
                .header(HOST, host)
                .json(&json!({ "client_name": "x", "redirect_uris": ["https://x/cb"] }))
                .send()
                .await
                .expect("send register");
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "register is tenant-only ({host})"
            );
        }

        h.stop().await;
    })
    .await;
}

#[actix_web::test]
async fn db_pinned_oauth_token_is_chokepoint_enforced() {
    with_control_db(
        "db_pinned_oauth_token_is_chokepoint_enforced",
        |url| async move {
            let h = OAuthHarness::spawn(&url).await;
            let account = h.provision("alpha").await;

            // The slice defaults MCP tokens to account scope, but a db-pinned
            // authorization must still be honored AND chokepoint-enforced.
            // Issue a db-pinned mcp token directly (the consent flow's
            // account-scope default is covered by the full-flow test).
            let pinned = issue_token(
                &h.control,
                &account.account_id,
                TokenScope::Mcp,
                Some("the-pinned-kb"),
                "mcp-oauth: pinned",
            )
            .await
            .expect("issue db-pinned mcp token");

            // A request selecting a DIFFERENT database is 403 — the db-scope
            // chokepoint (CloudAuth), proving a db-pinned MCP token can't read
            // another KB via header override.
            let resp = h
                .req(Method::GET, "alpha", "/api/atoms")
                .bearer_auth(&pinned)
                .header("X-Atomic-Database", "some-other-kb")
                .send()
                .await
                .expect("send api with override");
            assert_eq!(
                resp.status(),
                StatusCode::FORBIDDEN,
                "db-pinned token can't reach another KB"
            );

            h.stop().await;
        },
    )
    .await;
}

#[actix_web::test]
async fn mcp_token_is_tenant_isolated_across_subdomains() {
    with_control_db(
        "mcp_token_is_tenant_isolated_across_subdomains",
        |url| async move {
            let h = OAuthHarness::spawn(&url).await;
            let alpha = h.provision("alpha").await;
            let _bravo = h.provision("bravo").await;

            // An account-scope mcp token for alpha (the consent flow's default
            // — issued directly here; the full mint path is covered above).
            let token = issue_token(
                &h.control,
                &alpha.account_id,
                TokenScope::Mcp,
                None,
                "mcp-oauth: alpha",
            )
            .await
            .expect("issue alpha mcp token");

            // On alpha's own subdomain it initializes an MCP session: it
            // operates on alpha's knowledge base (CloudAuth resolves alpha's
            // tenant and injects its manager, which the transport resolves
            // per-request).
            let on_alpha = h.mcp_initialize("alpha", &token).await;
            assert_eq!(
                on_alpha.status(),
                StatusCode::OK,
                "alpha's mcp token works on alpha's /mcp"
            );
            assert!(
                on_alpha.headers().contains_key("mcp-session-id"),
                "mcp session established on the owning tenant"
            );

            // The SAME token on bravo's subdomain → 401: CloudAuth verifies
            // `WHERE account_id = bravo AND hash = ?`, and alpha's token hashes
            // to no bravo row (the cross-tenant chokepoint). It never reaches
            // bravo's knowledge base.
            let on_bravo = h.mcp_initialize("bravo", &token).await;
            assert_eq!(
                on_bravo.status(),
                StatusCode::UNAUTHORIZED,
                "alpha's mcp token is rejected on bravo's /mcp (cross-tenant)"
            );

            h.stop().await;
        },
    )
    .await;
}

#[actix_web::test]
async fn db_pinned_mcp_token_cannot_reach_another_kb_via_mcp() {
    with_control_db(
        "db_pinned_mcp_token_cannot_reach_another_kb_via_mcp",
        |url| async move {
            let h = OAuthHarness::spawn(&url).await;
            let account = h.provision("alpha").await;

            // A db-pinned mcp token: it may only touch `the-pinned-kb`.
            let pinned = issue_token(
                &h.control,
                &account.account_id,
                TokenScope::Mcp,
                Some("the-pinned-kb"),
                "mcp-oauth: pinned",
            )
            .await
            .expect("issue db-pinned mcp token");

            // Trying to reach a DIFFERENT KB on /mcp via the X-Atomic-Database
            // header → 403 at the CloudAuth chokepoint. Note this 403 is raised
            // by CloudAuth *before* the request reaches the MCP transport, so
            // this case exercises the explicit-different-db rejection only — NOT
            // the transport's own db resolution on the default (no-selection)
            // path. The positive pin (a no-selection request landing on the
            // pinned KB, which depends on the transport honoring the injected
            // X-Atomic-Database header) is proven by
            // `db_pinned_mcp_token_resolves_to_pinned_kb_not_active`.
            let resp = h
                .req(Method::POST, "alpha", "/mcp")
                .bearer_auth(&pinned)
                .header("X-Atomic-Database", "some-other-kb")
                .header("Accept", "application/json, text/event-stream")
                .header("Content-Type", "application/json")
                .body(
                    json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "initialize",
                        "params": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {},
                            "clientInfo": { "name": "claude", "version": "0" }
                        }
                    })
                    .to_string(),
                )
                .send()
                .await
                .expect("send mcp with db override");
            assert_eq!(
                resp.status(),
                StatusCode::FORBIDDEN,
                "db-pinned mcp token can't select another KB on /mcp"
            );

            h.stop().await;
        },
    )
    .await;
}

#[actix_web::test]
async fn db_pinned_mcp_token_resolves_to_pinned_kb_not_active() {
    // The positive db-pin case Issue 2 fixes: a tenant with two KBs, the
    // ACTIVE one (KB-A, the provisioned default) different from the pinned one
    // (KB-B). An MCP token pinned to KB-B, with NO `?db=` and NO header on the
    // request, must operate on KB-B — the pin CloudAuth injects as
    // `X-Atomic-Database` — and NOT fall through to the tenant's active KB-A.
    //
    // Before the fix the MCP transport read only `?db=` and ignored the
    // injected header, so a no-selection request resolved to KB-A (the active),
    // silently crossing the pin within the account.
    with_control_db(
        "db_pinned_mcp_token_resolves_to_pinned_kb_not_active",
        |url| async move {
            let h = OAuthHarness::spawn(&url).await;
            let account = h.provision("alpha").await;

            // KB-A is the provisioned default and stays the ACTIVE KB. Create
            // KB-B as a second KB (creation does not change the active KB).
            let kb_b = h
                .create_kb("alpha", &account.account_token, "Second KB")
                .await;

            // Seed a distinct, identifiable atom in each KB.
            let atom_a = h
                .seed_atom("alpha", &account.account_token, "ACTIVE-KB-A-ATOM-BODY")
                .await;
            let atom_b = h
                .seed_atom_in_db(
                    "alpha",
                    &account.account_token,
                    &kb_b,
                    "PINNED-KB-B-ATOM-BODY",
                )
                .await;

            // An MCP token PINNED to the non-active KB-B.
            let pinned = issue_token(
                &h.control,
                &account.account_id,
                TokenScope::Mcp,
                Some(&kb_b),
                "mcp-oauth: pinned to KB-B",
            )
            .await
            .expect("issue KB-B-pinned mcp token");

            // read_atom on KB-B's atom with NO ?db= and NO header: the only way
            // this resolves is if the transport honors the X-Atomic-Database pin
            // CloudAuth injected for the unselective request → operates on KB-B.
            let text_b = h
                .mcp_tools_call("alpha", &pinned, "read_atom", json!({ "atom_id": atom_b }))
                .await;
            assert!(
                text_b.contains("PINNED-KB-B-ATOM-BODY"),
                "no-selection request on a KB-B-pinned token resolves to KB-B: {text_b}"
            );

            // The active KB-A's atom is NOT visible through the pinned token —
            // proving the request landed on KB-B (the pin), not KB-A (the
            // active fallback the pre-fix transport would have chosen).
            let text_a = h
                .mcp_tools_call("alpha", &pinned, "read_atom", json!({ "atom_id": atom_a }))
                .await;
            assert!(
                text_a.contains("Atom not found"),
                "the pin keeps KB-A's atom out of reach (not the active fallback): {text_a}"
            );

            h.stop().await;
        },
    )
    .await;
}

#[actix_web::test]
async fn unauthenticated_mcp_returns_401_pointing_at_tenant_discovery() {
    with_control_db(
        "unauthenticated_mcp_returns_401_pointing_at_tenant_discovery",
        |url| async move {
            let h = OAuthHarness::spawn(&url).await;
            let _account = h.provision("alpha").await;

            // No credential on /mcp → 401 carrying the MCP-compliant
            // WWW-Authenticate challenge pointing at alpha's OWN OAuth
            // protected-resource metadata, so Claude Desktop discovers the
            // cloud OAuth flow for this exact tenant.
            let resp = h
                .req(Method::POST, "alpha", "/mcp")
                .header("Accept", "application/json, text/event-stream")
                .header("Content-Type", "application/json")
                .body(
                    json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "initialize",
                        "params": {}
                    })
                    .to_string(),
                )
                .send()
                .await
                .expect("send unauthenticated mcp");
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
            let challenge = resp
                .headers()
                .get(WWW_AUTHENTICATE)
                .expect("unauthenticated /mcp must carry WWW-Authenticate")
                .to_str()
                .expect("challenge str");
            assert!(
                challenge.starts_with("Bearer "),
                "Bearer challenge: {challenge}"
            );
            assert!(
                challenge.contains("resource_metadata="),
                "challenge carries resource_metadata: {challenge}"
            );
            assert!(
                challenge
                    .contains("http://alpha.cloudtest.local/.well-known/oauth-protected-resource"),
                "resource_metadata points at alpha's own discovery: {challenge}"
            );

            // The pointed-at discovery actually resolves (proving the client
            // following the challenge reaches alpha's OAuth metadata).
            let meta: Value = h
                .req(
                    Method::GET,
                    "alpha",
                    "/.well-known/oauth-protected-resource",
                )
                .send()
                .await
                .expect("send discovery")
                .json()
                .await
                .expect("discovery json");
            assert_eq!(meta["resource"], "http://alpha.cloudtest.local/mcp");

            // A trailing-slash `/mcp/` (which some clients send before path
            // normalization) gets the SAME challenge — the decoration matches
            // the bare path and anything beneath it, so a trailing-slash client
            // still discovers the OAuth flow rather than seeing a bare 401.
            let slash = h
                .req(Method::POST, "alpha", "/mcp/")
                .header("Accept", "application/json, text/event-stream")
                .header("Content-Type", "application/json")
                .body(
                    json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} })
                        .to_string(),
                )
                .send()
                .await
                .expect("send unauthenticated mcp trailing slash");
            assert_eq!(slash.status(), StatusCode::UNAUTHORIZED);
            assert!(
                slash
                    .headers()
                    .get(WWW_AUTHENTICATE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|c| c.starts_with("Bearer ") && c.contains("resource_metadata=")),
                "/mcp/ 401 carries the WWW-Authenticate challenge"
            );

            // The /api data plane, by contrast, gets a plain 401 — no MCP
            // discovery noise leaks onto it.
            let api = h
                .req(Method::GET, "alpha", "/api/atoms")
                .send()
                .await
                .expect("send unauthenticated api");
            assert_eq!(api.status(), StatusCode::UNAUTHORIZED);
            assert!(
                !api.headers().contains_key(WWW_AUTHENTICATE),
                "the data plane 401 carries no MCP discovery challenge"
            );

            h.stop().await;
        },
    )
    .await;
}
