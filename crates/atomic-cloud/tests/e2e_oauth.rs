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

        Account {
            account_id: account.account_id,
        }
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
            // header → 403 at the CloudAuth chokepoint, before the request ever
            // reaches the MCP transport's manager resolution. This proves the
            // MCP path is governed by the SAME allowed_db_id chokepoint the
            // data plane uses — a db-pinned MCP token can't read another KB
            // through the MCP context's db selection.
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
