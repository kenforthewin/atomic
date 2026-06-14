//! End-to-end tests for the app-host account plane: the host-based plane
//! split (both fail-closed directions), signup/login request-link behavior,
//! login indistinguishability, the anti-abuse rate limits, and the
//! completion flows (consume → provision → session cookie → redirect).
//!
//! Each test spawns the real composition — `configure_cloud_app` on an
//! ephemeral port, exactly as `atomic-cloud serve` wires it — with a
//! capturing email sender (NO REAL EMAIL, EVER) and drives it with explicit
//! `Host` headers: `cloudtest.local` / `app.cloudtest.local` for the
//! account plane, `<subdomain>.cloudtest.local` for tenants. The harness
//! client never follows redirects: completion responses are asserted on the
//! raw 302 + `Set-Cookie` header strings (a cookie jar would silently drop
//! the `Secure` cookie over plain-HTTP loopback).
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command.

mod support;

use std::sync::Arc;
use std::time::Duration;

use actix_web::{App, HttpServer};
use atomic_cloud::{
    configure_cloud_app, issue_token, provision_account, AccountCache, AccountCacheConfig,
    AccountPlane, AccountPlaneConfig, ChatStreamLimiter, CloudAuth, ClusterConfig, ControlPlane,
    FallbackAppState, MagicLinkPurpose, ManagedKeys, NewAccount, QuotaBilling, RateLimits,
    Readiness, TenantPlane, TokenScope, DEFAULT_CHAT_STREAMS_PER_ACCOUNT, SESSION_COOKIE,
};
use reqwest::header::{HOST, LOCATION, RETRY_AFTER, SET_COOKIE};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use support::{control_db_contains, with_control_db, CapturingSender, DelayedSender, SentEmail};

/// Base domain the composition is configured with. The app host is this
/// name itself and `app.<BASE_DOMAIN>`.
const BASE_DOMAIN: &str = "cloudtest.local";

fn sha256_hex(plaintext: &str) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(plaintext.as_bytes()))
}

/// Plane config for tests: production defaults under [`BASE_DOMAIN`] with
/// the given rate limits.
fn plane_config(rate_limits: RateLimits) -> AccountPlaneConfig {
    AccountPlaneConfig {
        rate_limits,
        ..AccountPlaneConfig::new(BASE_DOMAIN)
    }
}

/// The composed cloud server on an ephemeral port, with the account plane
/// backed by a capturing sender.
struct PlaneHarness {
    control: ControlPlane,
    cluster: ClusterConfig,
    sender: CapturingSender,
    /// The live plane, kept so tests can reach the provision semaphore.
    plane: AccountPlane,
    client: reqwest::Client,
    base_url: String,
    handle: actix_web::dev::ServerHandle,
    /// Owns the scratch directory behind the inert fallback `AppState`;
    /// must outlive the server.
    _fallback: FallbackAppState,
}

impl PlaneHarness {
    async fn spawn(control_url: &str, config: AccountPlaneConfig) -> Self {
        let sender = CapturingSender::default();
        let email: Arc<dyn atomic_cloud::EmailSender> = Arc::new(sender.clone());
        Self::spawn_with_sender(control_url, config, email, sender).await
    }

    /// [`Self::spawn`] with an explicit [`EmailSender`] (e.g. a
    /// [`DelayedSender`] for timing assertions). `sender` must be the
    /// capture the custom sender ultimately records into.
    async fn spawn_with_sender(
        control_url: &str,
        config: AccountPlaneConfig,
        email: Arc<dyn atomic_cloud::EmailSender>,
        sender: CapturingSender,
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
        let account_plane = AccountPlane::new(
            control.clone(),
            cluster.clone(),
            ManagedKeys::Disabled,
            email,
            config,
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
        let oauth_plane = atomic_cloud::OAuthPlane::new(
            control.clone(),
            BASE_DOMAIN,
            "http",
            format!("http://app.{BASE_DOMAIN}"),
        );
        let mcp_transport = fallback.mcp_transport(atomic_cloud::DEFAULT_MCP_SSE_KEEP_ALIVE);
        let plane = account_plane.clone();
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
                None,
            ))
        })
        .workers(1)
        .listen(listener)
        .expect("attach listener")
        .run();
        let handle = server.handle();
        actix_web::rt::spawn(server);

        PlaneHarness {
            control,
            cluster,
            sender,
            plane,
            // Never follow redirects: completion 302s point at
            // `<slug>.cloudtest.local`, which doesn't resolve — and the
            // assertions are on the raw headers anyway.
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("build http client"),
            base_url: format!("http://127.0.0.1:{port}"),
            handle,
            _fallback: fallback,
        }
    }

    async fn stop(self) {
        self.handle.stop(false).await;
    }

    /// Request builder with an explicit `Host` header over the loopback
    /// listener.
    fn on_host(&self, method: Method, host: &str, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base_url))
            .header(HOST, host)
    }

    async fn request_signup_link(
        &self,
        host: &str,
        email: &str,
        subdomain: &str,
    ) -> reqwest::Response {
        self.on_host(Method::POST, host, "/signup/request-link")
            .json(&json!({ "email": email, "subdomain": subdomain }))
            .send()
            .await
            .expect("send signup request-link")
    }

    async fn request_login_link(&self, email: &str) -> reqwest::Response {
        self.on_host(
            Method::POST,
            &format!("app.{BASE_DOMAIN}"),
            "/login/request-link",
        )
        .json(&json!({ "email": email }))
        .send()
        .await
        .expect("send login request-link")
    }

    /// GET a completion route on the app host. `kind` is `signup` or
    /// `login`; the client never follows the resulting redirect.
    async fn complete(&self, kind: &str, token: &str) -> reqwest::Response {
        self.on_host(
            Method::GET,
            &format!("app.{BASE_DOMAIN}"),
            &format!("/{kind}/complete?token={token}"),
        )
        .send()
        .await
        .expect("send complete")
    }

    /// Request a signup link and return the captured token (the most
    /// recently "sent" email's).
    async fn signup_token(&self, email: &str, subdomain: &str) -> String {
        let resp = self
            .request_signup_link(&format!("app.{BASE_DOMAIN}"), email, subdomain)
            .await;
        assert_eq!(resp.status(), StatusCode::OK, "request signup link");
        let sent = self.sender.sent();
        let last = sent.last().expect("a signup email was captured");
        assert_eq!(last.to, email);
        token_from_link(&last.link).to_string()
    }

    /// Request a login link and return the captured token. The login route
    /// spawns its issue+send fire-and-forget (the timing-uniformity fix),
    /// so the capture is polled briefly rather than read synchronously.
    async fn login_token(&self, email: &str) -> String {
        let already_sent = self.sender.sent().len();
        let resp = self.request_login_link(email).await;
        assert_eq!(resp.status(), StatusCode::OK, "request login link");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let sent = self.sender.sent();
            if sent.len() > already_sent {
                let last = sent.last().expect("len > already_sent");
                assert_eq!(last.to, email);
                assert_eq!(last.purpose, MagicLinkPurpose::Login);
                return token_from_link(&last.link).to_string();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "a login email was never captured"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Whether the magic-link row for `token` has been consumed.
    async fn link_consumed(&self, token: &str) -> bool {
        let consumed: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT consumed_at FROM magic_links WHERE token_hash = $1")
                .bind(sha256_hex(token))
                .fetch_one(self.control.pool())
                .await
                .expect("magic link row exists");
        consumed.is_some()
    }

    /// `accounts` rows for an email, any status.
    async fn account_count(&self, email: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM accounts WHERE email = $1")
            .bind(email)
            .fetch_one(self.control.pool())
            .await
            .expect("count accounts")
    }
}

/// Pull the `token=` value out of a captured link.
fn token_from_link(link: &str) -> &str {
    link.split("token=").nth(1).expect("link carries a token")
}

/// The `Set-Cookie` header of a completion response, as the raw string —
/// assertions run on the string because a cookie jar would drop the
/// `Secure` cookie over plain HTTP.
fn set_cookie_header(resp: &reqwest::Response) -> String {
    resp.headers()
        .get(SET_COOKIE)
        .expect("response carries Set-Cookie")
        .to_str()
        .expect("Set-Cookie is ascii")
        .to_string()
}

/// The session secret inside a `Set-Cookie` string.
fn cookie_session_value(set_cookie: &str) -> &str {
    set_cookie
        .strip_prefix(&format!("{SESSION_COOKIE}="))
        .expect("cookie is the session cookie")
        .split(';')
        .next()
        .expect("cookie has a value")
}

/// Assert the full attribute set from the plan ("Web sessions") on a raw
/// `Set-Cookie` string. The cookie crate serializes the domain without the
/// leading dot (RFC 6265 ignores it), so both spellings are accepted.
fn assert_session_cookie_attributes(set_cookie: &str) {
    assert!(
        set_cookie.starts_with(&format!("{SESSION_COOKIE}=ats_")),
        "cookie must carry an ats_ session secret: {set_cookie}"
    );
    assert!(
        set_cookie.contains(&format!("Domain={BASE_DOMAIN}"))
            || set_cookie.contains(&format!("Domain=.{BASE_DOMAIN}")),
        "cookie must be scoped to the base domain: {set_cookie}"
    );
    for attribute in ["Secure", "HttpOnly", "SameSite=Lax", "Path=/"] {
        assert!(
            set_cookie.contains(attribute),
            "cookie must carry {attribute}: {set_cookie}"
        );
    }
    // Max-Age matches the default session TTL (30 days).
    assert!(
        set_cookie.contains("Max-Age=2592000"),
        "cookie Max-Age must match the session TTL: {set_cookie}"
    );
}

// ==================== Plane split ====================

/// Fail-closed direction one: the app host (bare base domain AND
/// `app.<base>`) must 404 every tenant route, even with a perfectly valid
/// tenant credential attached.
#[actix_web::test]
async fn app_host_404s_tenant_routes() {
    with_control_db("app_host_404s_tenant_routes", |url| async move {
        let h = PlaneHarness::spawn(&url, plane_config(RateLimits::default())).await;
        let account = provision_account(
            &h.control,
            &h.cluster,
            &ManagedKeys::Disabled,
            NewAccount {
                email: "alpha@example.com".to_string(),
                subdomain: "alpha".to_string(),
            },
        )
        .await
        .expect("provision alpha");
        let token = issue_token(
            &h.control,
            &account.account_id,
            TokenScope::Account,
            None,
            "e2e",
        )
        .await
        .expect("issue token");

        for host in [BASE_DOMAIN.to_string(), format!("app.{BASE_DOMAIN}")] {
            for (method, path) in [
                (Method::GET, "/api/atoms"),
                (Method::POST, "/api/atoms"),
                (Method::GET, "/api/databases"),
                (Method::GET, "/api/tags"),
                (Method::GET, "/ws"),
            ] {
                let resp = h
                    .on_host(method.clone(), &host, path)
                    .bearer_auth(&token)
                    .send()
                    .await
                    .expect("send");
                assert_eq!(
                    resp.status(),
                    StatusCode::NOT_FOUND,
                    "{method} {path} on app host {host} must be 404"
                );
            }

            // …while the account plane serves on the same host.
            let resp = h
                .request_signup_link(&host, "someone@example.com", &format!("ok-{}", host.len()))
                .await;
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "account plane must serve on {host}"
            );
        }

        // Sanity: the tenant route works where it belongs.
        let resp = h
            .on_host(Method::GET, &format!("alpha.{BASE_DOMAIN}"), "/api/atoms")
            .bearer_auth(&token)
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), StatusCode::OK);

        h.stop().await;
    })
    .await;
}

/// Fail-closed direction two: tenant subdomains (existing or not) must 404
/// every account-plane route — the routes don't exist off the app host.
#[actix_web::test]
async fn tenant_subdomains_404_account_plane_routes() {
    with_control_db(
        "tenant_subdomains_404_account_plane_routes",
        |url| async move {
            let h = PlaneHarness::spawn(&url, plane_config(RateLimits::default())).await;
            let account = provision_account(
                &h.control,
                &h.cluster,
                &ManagedKeys::Disabled,
                NewAccount {
                    email: "alpha@example.com".to_string(),
                    subdomain: "alpha".to_string(),
                },
            )
            .await
            .expect("provision alpha");
            let token = issue_token(
                &h.control,
                &account.account_id,
                TokenScope::Account,
                None,
                "e2e",
            )
            .await
            .expect("issue token");

            // A real tenant's subdomain, with and without credentials, and a
            // ghost subdomain: none of them carries the account plane.
            for host in [
                format!("alpha.{BASE_DOMAIN}"),
                format!("ghost.{BASE_DOMAIN}"),
            ] {
                for (path, body) in [
                    (
                        "/signup/request-link",
                        json!({ "email": "x@example.com", "subdomain": "fresh" }),
                    ),
                    ("/login/request-link", json!({ "email": "x@example.com" })),
                ] {
                    let resp = h
                        .on_host(Method::POST, &host, path)
                        .bearer_auth(&token)
                        .json(&body)
                        .send()
                        .await
                        .expect("send");
                    assert_eq!(
                        resp.status(),
                        StatusCode::NOT_FOUND,
                        "POST {path} on tenant host {host} must be 404"
                    );
                }
            }
            assert!(
                h.sender.sent().is_empty(),
                "no email may result from requests off the app host"
            );

            h.stop().await;
        },
    )
    .await;
}

// ==================== Signup request-link ====================

/// The full signup request-link happy path: 200, exactly one captured email
/// whose link points at the app host's complete route, a hash-only
/// magic_links row recording the request, and no `aml_` substring anywhere
/// in the control database.
#[actix_web::test]
async fn signup_request_link_end_to_end() {
    with_control_db("signup_request_link_end_to_end", |url| async move {
        let h = PlaneHarness::spawn(&url, plane_config(RateLimits::default())).await;

        let resp = h
            .request_signup_link(&format!("app.{BASE_DOMAIN}"), "kenny@example.com", "kenny")
            .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let sent = h.sender.sent();
        assert_eq!(sent.len(), 1, "exactly one email per request");
        let SentEmail { to, link, purpose } = &sent[0];
        assert_eq!(to, "kenny@example.com");
        assert_eq!(*purpose, MagicLinkPurpose::Signup);
        assert!(
            link.starts_with(&format!(
                "https://app.{BASE_DOMAIN}/signup/complete?token=aml_"
            )),
            "link must point at the app host's signup completion: {link}"
        );

        // The stored row is the request, keyed by the token's hash.
        let token = token_from_link(link);
        let (purpose, subdomain, ip): (String, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT purpose, requested_subdomain, request_ip FROM magic_links \
             WHERE token_hash = $1",
        )
        .bind(sha256_hex(token))
        .fetch_one(h.control.pool())
        .await
        .expect("row exists under the emailed token's hash");
        assert_eq!(purpose, "signup");
        assert_eq!(subdomain.as_deref(), Some("kenny"));
        assert_eq!(
            ip.as_deref(),
            Some("127.0.0.1"),
            "the peer address is recorded as the request IP"
        );

        // Hash-only, end to end: the emailed plaintext appears nowhere in
        // the control database.
        assert!(
            !control_db_contains(&url, "aml_").await,
            "no aml_ substring may appear anywhere in the control database"
        );

        h.stop().await;
    })
    .await;
}

/// Validation failures are honest 400s with typed errors — and produce no
/// email and no magic_links row.
#[actix_web::test]
async fn signup_validation_errors_are_honest_400s() {
    with_control_db(
        "signup_validation_errors_are_honest_400s",
        |url| async move {
            // Every request below comes from one IP (and several reuse one
            // email); raise the limits so this test exercises validation
            // only — the limiter has its own tests.
            let h = PlaneHarness::spawn(
                &url,
                plane_config(RateLimits {
                    links_per_ip: 100,
                    links_per_email: 100,
                    ..RateLimits::default()
                }),
            )
            .await;
            // An existing account and an active reservation to collide with.
            provision_account(
                &h.control,
                &h.cluster,
                &ManagedKeys::Disabled,
                NewAccount {
                    email: "alpha@example.com".to_string(),
                    subdomain: "alpha".to_string(),
                },
            )
            .await
            .expect("provision alpha");
            sqlx::query(
                "INSERT INTO subdomains_reserved (subdomain, expires_at) \
                 VALUES ('parked', NOW() + INTERVAL '90 days')",
            )
            .execute(h.control.pool())
            .await
            .expect("park subdomain");

            let app_host = format!("app.{BASE_DOMAIN}");
            for (email, subdomain, expected_error) in [
                ("not-an-email", "fine-slug", "invalid_email"),
                ("k@example.com", "ab", "invalid_subdomain"),
                ("k@example.com", "Has-Upper", "invalid_subdomain"),
                ("k@example.com", "admin", "subdomain_reserved"),
                ("k@example.com", "parked", "subdomain_reserved"),
                ("k@example.com", "alpha", "subdomain_taken"),
            ] {
                let resp = h.request_signup_link(&app_host, email, subdomain).await;
                assert_eq!(
                    resp.status(),
                    StatusCode::BAD_REQUEST,
                    "({email}, {subdomain}) must be an honest 400"
                );
                let body: Value = resp.json().await.expect("error json");
                assert_eq!(
                    body["error"], expected_error,
                    "({email}, {subdomain}) error code"
                );
            }

            assert!(
                h.sender.sent().is_empty(),
                "validation failures must not send email"
            );
            let links: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM magic_links")
                .fetch_one(h.control.pool())
                .await
                .expect("count links");
            assert_eq!(links, 0, "validation failures must not issue links");

            h.stop().await;
        },
    )
    .await;
}

// ==================== Login request-link ====================

/// No email enumeration: the response to a login request-link is
/// byte-identical whether or not an active account matches the email — only
/// the side effects differ (one email for the real account, none for the
/// ghost).
#[actix_web::test]
async fn login_request_link_is_indistinguishable() {
    with_control_db(
        "login_request_link_is_indistinguishable",
        |url| async move {
            let h = PlaneHarness::spawn(&url, plane_config(RateLimits::default())).await;
            provision_account(
                &h.control,
                &h.cluster,
                &ManagedKeys::Disabled,
                NewAccount {
                    email: "alpha@example.com".to_string(),
                    subdomain: "alpha".to_string(),
                },
            )
            .await
            .expect("provision alpha");

            let real = h.request_login_link("alpha@example.com").await;
            let real_status = real.status();
            let real_body = real.bytes().await.expect("body");

            let ghost = h.request_login_link("ghost@example.com").await;
            let ghost_status = ghost.status();
            let ghost_body = ghost.bytes().await.expect("body");

            assert_eq!(real_status, StatusCode::OK);
            assert_eq!(
                real_status, ghost_status,
                "status must not reveal account existence"
            );
            assert_eq!(
                real_body, ghost_body,
                "body must be byte-identical for existing and unknown emails"
            );

            // Side effects: exactly one login email, for the real account,
            // pointing at the login completion route. The send is spawned
            // fire-and-forget (the timing-uniformity fix), so wait briefly
            // for it to land before asserting.
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while h.sender.sent().is_empty() && std::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let sent = h.sender.sent();
            assert_eq!(sent.len(), 1, "only the real account gets an email");
            assert_eq!(sent[0].to, "alpha@example.com");
            assert_eq!(sent[0].purpose, MagicLinkPurpose::Login);
            assert!(sent[0].link.starts_with(&format!(
                "https://app.{BASE_DOMAIN}/login/complete?token=aml_"
            )));
            let row: (String, Option<String>) = sqlx::query_as(
                "SELECT purpose, requested_subdomain FROM magic_links WHERE token_hash = $1",
            )
            .bind(sha256_hex(token_from_link(&sent[0].link)))
            .fetch_one(h.control.pool())
            .await
            .expect("login link row");
            assert_eq!(row.0, "login");
            assert_eq!(row.1, None, "login links carry no subdomain");
            let links: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM magic_links")
                .fetch_one(h.control.pool())
                .await
                .expect("count links");
            assert_eq!(links, 1, "no row may be issued for the unknown email");

            h.stop().await;
        },
    )
    .await;
}

/// Timing-oracle regression: the login route must NOT await the issue+send
/// on the exists branch (an outbound email send takes hundreds of ms in
/// production; awaiting it only when the account exists hands out the very
/// signal the byte-identical bodies hide). With a sender that takes 3 s to
/// "deliver", the response for a real account still arrives in a fraction
/// of that, and the link lands in the capture afterwards — and works. The
/// bounds are deliberately loose (1.5 s response vs. 3 s sender, 15 s
/// capture deadline) to avoid flake.
#[actix_web::test]
async fn login_request_link_returns_before_the_send() {
    with_control_db(
        "login_request_link_returns_before_the_send",
        |url| async move {
            let capture = CapturingSender::default();
            let delay = Duration::from_secs(3);
            let h = PlaneHarness::spawn_with_sender(
                &url,
                plane_config(RateLimits::default()),
                Arc::new(DelayedSender {
                    inner: capture.clone(),
                    delay,
                }),
                capture.clone(),
            )
            .await;
            provision_account(
                &h.control,
                &h.cluster,
                &ManagedKeys::Disabled,
                NewAccount {
                    email: "alpha@example.com".to_string(),
                    subdomain: "alpha".to_string(),
                },
            )
            .await
            .expect("provision alpha");

            let started = std::time::Instant::now();
            let resp = h.request_login_link("alpha@example.com").await;
            let elapsed = started.elapsed();
            assert_eq!(resp.status(), StatusCode::OK);
            assert!(
                elapsed < Duration::from_millis(1500),
                "the exists branch must return without awaiting the send; took {elapsed:?}"
            );
            assert!(
                capture.sent().is_empty(),
                "the slow send must still be in flight when the response lands"
            );

            // The spawned send completes on its own, and its link is a
            // working credential.
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            while capture.sent().is_empty() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the spawned send never completed"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let sent = capture.sent();
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0].to, "alpha@example.com");
            assert_eq!(sent[0].purpose, MagicLinkPurpose::Login);
            let token = token_from_link(&sent[0].link).to_string();
            let resp = h.complete("login", &token).await;
            assert_eq!(
                resp.status(),
                StatusCode::FOUND,
                "the fire-and-forget link must complete a login"
            );

            h.stop().await;
        },
    )
    .await;
}

// ==================== Rate limits ====================

/// The per-IP request-link limit admits exactly `limit` requests, refuses
/// the next with 429 + Retry-After, and admits again once the window
/// passes. Validation-failing requests count as attempts (they're charged
/// before validation), pinned by spending one slot on a bad slug — and the
/// bucket is SHARED across the signup and login routes, pinned by the
/// refusal landing on whichever route is hit next.
#[actix_web::test]
async fn ip_rate_limit_spans_routes_enforces_and_resets() {
    with_control_db(
        "ip_rate_limit_spans_routes_enforces_and_resets",
        |url| async move {
            let window = Duration::from_millis(1500);
            let h = PlaneHarness::spawn(
                &url,
                plane_config(RateLimits {
                    links_per_ip: 3,
                    ip_window: window,
                    // Distinct emails below keep the email limit out of play.
                    ..RateLimits::default()
                }),
            )
            .await;
            let app_host = format!("app.{BASE_DOMAIN}");

            // Admission 1 — a validation failure still burns a slot.
            let resp = h
                .request_signup_link(&app_host, "a@example.com", "ab")
                .await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            // Admission 2 — a login request draws from the same bucket.
            let resp = h.request_login_link("b@example.com").await;
            assert_eq!(resp.status(), StatusCode::OK);
            // Admission 3.
            let resp = h
                .request_signup_link(&app_host, "c@example.com", "slug-c")
                .await;
            assert_eq!(resp.status(), StatusCode::OK);

            // Over the limit: 429 with Retry-After — on BOTH routes, since
            // the bucket is shared (switching routes mints no allowance).
            let resp = h
                .request_signup_link(&app_host, "d@example.com", "slug-d")
                .await;
            assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
            let retry_after: u64 = resp
                .headers()
                .get(RETRY_AFTER)
                .expect("429 carries Retry-After")
                .to_str()
                .expect("header is ascii")
                .parse()
                .expect("Retry-After is integer seconds");
            assert!(retry_after >= 1, "rounded up, never zero");
            let body: Value = resp.json().await.expect("denial json");
            assert_eq!(body["error"], "rate_limited");
            assert_eq!(body["retry_after_seconds"], retry_after);
            let resp = h.request_login_link("e@example.com").await;
            assert_eq!(
                resp.status(),
                StatusCode::TOO_MANY_REQUESTS,
                "the login route must refuse from the same exhausted bucket"
            );
            assert!(resp.headers().get(RETRY_AFTER).is_some());
            assert_eq!(
                h.sender.sent().len(),
                1,
                "exactly one email: the admitted signup (the admitted login \
                 was a ghost email; the refused requests sent nothing)"
            );

            // After the window the limiter resets, for both routes.
            tokio::time::sleep(window + Duration::from_millis(200)).await;
            let resp = h
                .request_signup_link(&app_host, "d@example.com", "slug-d")
                .await;
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "limit must reset once the window passes"
            );
            let resp = h.request_login_link("f@example.com").await;
            assert_eq!(resp.status(), StatusCode::OK);

            h.stop().await;
        },
    )
    .await;
}

/// The per-email limit (3/hour in production; shrunk here) spans signup and
/// login: requests for one email are admitted `limit` times across both
/// routes, refused with 429 afterwards, and admitted again after the
/// window. A different email is unaffected throughout.
#[actix_web::test]
async fn email_rate_limit_enforces_and_resets() {
    with_control_db("email_rate_limit_enforces_and_resets", |url| async move {
        let window = Duration::from_millis(1500);
        let h = PlaneHarness::spawn(
            &url,
            plane_config(RateLimits {
                links_per_email: 2,
                email_window: window,
                // Every request below shares one loopback IP; keep the
                // (now route-spanning) IP bucket out of play.
                links_per_ip: 100,
                ..RateLimits::default()
            }),
        )
        .await;
        provision_account(
            &h.control,
            &h.cluster,
            &ManagedKeys::Disabled,
            NewAccount {
                email: "alpha@example.com".to_string(),
                subdomain: "alpha".to_string(),
            },
        )
        .await
        .expect("provision alpha");
        let app_host = format!("app.{BASE_DOMAIN}");

        // Admission 1 (login) and 2 (signup — the limit is per email across
        // both routes; case differences don't mint extra allowance).
        let resp = h.request_login_link("alpha@example.com").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = h
            .request_signup_link(&app_host, "Alpha@Example.com", "second-kb")
            .await;
        assert_eq!(resp.status(), StatusCode::OK);

        // Third request for the same email: refused, on either route.
        let resp = h.request_login_link("alpha@example.com").await;
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(resp.headers().get(RETRY_AFTER).is_some());

        // Another email is its own bucket.
        let resp = h.request_login_link("bravo@example.com").await;
        assert_eq!(resp.status(), StatusCode::OK);

        // After the window the email admits again.
        tokio::time::sleep(window + Duration::from_millis(200)).await;
        let resp = h.request_login_link("alpha@example.com").await;
        assert_eq!(resp.status(), StatusCode::OK);

        h.stop().await;
    })
    .await;
}

// ==================== Signup completion ====================

/// The full signup happy path over real HTTP: request a link, pull it from
/// the capturing sender, GET the completion route, and assert the plan's
/// step-12 contract — 302 to the new tenant's subdomain, the session cookie
/// with the full attribute set (asserted on the raw header string; a cookie
/// jar would drop the `Secure` cookie over plain HTTP), an `active` account
/// with its tenant mapping, and the session cookie authenticating an API
/// call on the new subdomain. Hash-only discipline holds end to end: no
/// `ats_` (or `aml_`) plaintext anywhere in the control database.
#[actix_web::test]
async fn signup_complete_end_to_end() {
    with_control_db("signup_complete_end_to_end", |url| async move {
        let h = PlaneHarness::spawn(&url, plane_config(RateLimits::default())).await;
        let token = h.signup_token("kenny@example.com", "kenny").await;

        let resp = h.complete("signup", &token).await;
        assert_eq!(resp.status(), StatusCode::FOUND, "completion redirects");
        assert_eq!(
            resp.headers()
                .get(LOCATION)
                .expect("302 carries Location")
                .to_str()
                .expect("ascii"),
            &format!("https://kenny.{BASE_DOMAIN}/"),
            "redirect lands on the new tenant's subdomain"
        );
        let cookie = set_cookie_header(&resp);
        assert_session_cookie_attributes(&cookie);
        let session = cookie_session_value(&cookie).to_string();

        // The link is spent and the account is fully provisioned.
        assert!(
            h.link_consumed(&token).await,
            "completion consumes the link"
        );
        let (account_id, status): (String, String) =
            sqlx::query_as("SELECT id, status FROM accounts WHERE subdomain = 'kenny'")
                .fetch_one(h.control.pool())
                .await
                .expect("account row exists");
        assert_eq!(status, "active");
        let mappings: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM account_databases WHERE account_id = $1")
                .bind(&account_id)
                .fetch_one(h.control.pool())
                .await
                .expect("count mappings");
        assert_eq!(mappings, 1, "exactly one tenant database is recorded");

        // Signup starts the free trial (plan: "Trials: 14 days of paid tier
        // on signup, no card required"): the account lands on the paid tier
        // in the `trialing` state with a deadline ~14 days out — full access,
        // auto-downgraded by the dunning/trial sweep when it expires.
        let (billing_state, plan_id, trial_ends_at): (
            String,
            Option<String>,
            Option<chrono::DateTime<chrono::Utc>>,
        ) = sqlx::query_as(
            "SELECT billing_state, plan_id, trial_ends_at FROM accounts WHERE id = $1",
        )
        .bind(&account_id)
        .fetch_one(h.control.pool())
        .await
        .expect("billing columns");
        assert_eq!(billing_state, "trialing", "signup starts a trial");
        assert_eq!(
            plan_id.as_deref(),
            Some("pro"),
            "trial grants the paid tier"
        );
        let in_days = (trial_ends_at.expect("trial deadline") - chrono::Utc::now()).num_days();
        assert!(
            (12..=14).contains(&in_days),
            "trial deadline ~14 days out, got {in_days}"
        );

        // Hash-only, end to end: neither the session secret in the cookie
        // nor any link plaintext was ever persisted.
        assert!(
            !control_db_contains(&url, "ats_").await,
            "no ats_ substring may appear anywhere in the control database"
        );

        // The cookie is a working credential on the new subdomain.
        let resp = h
            .on_host(Method::GET, &format!("kenny.{BASE_DOMAIN}"), "/api/atoms")
            .header("Cookie", format!("{SESSION_COOKIE}={session}"))
            .send()
            .await
            .expect("send authenticated call");
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "the flow-issued session must authenticate tenant API calls"
        );

        h.stop().await;
    })
    .await;
}

/// With `cookie_secure: false` (the `--dangerously-insecure-cookies` dev flag,
/// for a headless box served over plain HTTP on a non-`localhost` host where a
/// `Secure` cookie would be silently dropped) the session cookie omits the
/// `Secure` attribute but keeps every other protection (`HttpOnly`,
/// `SameSite=Lax`, the base-domain scope). The default stays `Secure`
/// (asserted by `signup_complete_end_to_end`); this pins the opt-out.
#[actix_web::test]
async fn insecure_cookie_flag_drops_secure_only() {
    with_control_db("insecure_cookie_flag_drops_secure_only", |url| async move {
        let config = AccountPlaneConfig {
            cookie_secure: false,
            ..plane_config(RateLimits::default())
        };
        let h = PlaneHarness::spawn(&url, config).await;
        let token = h.signup_token("kenny@example.com", "kenny").await;

        let resp = h.complete("signup", &token).await;
        assert_eq!(resp.status(), StatusCode::FOUND);
        let cookie = set_cookie_header(&resp);

        assert!(
            !cookie.contains("Secure"),
            "the insecure-cookie flag must drop the Secure attribute: {cookie}"
        );
        // Every other protection is unchanged.
        for attribute in ["HttpOnly", "SameSite=Lax", "Path=/"] {
            assert!(
                cookie.contains(attribute),
                "cookie must still carry {attribute}: {cookie}"
            );
        }
        assert!(
            cookie.contains(&format!("Domain={BASE_DOMAIN}"))
                || cookie.contains(&format!("Domain=.{BASE_DOMAIN}")),
            "cookie must still be base-domain scoped: {cookie}"
        );

        h.stop().await;
    })
    .await;
}

/// Double-click safety: the second consumption of the same signup link is a
/// clean `invalid_link` 400 (the consume UPDATE matched zero rows) — never
/// a second provision. Account and tenant-mapping rows stay singular.
#[actix_web::test]
async fn signup_link_is_single_use() {
    with_control_db("signup_link_is_single_use", |url| async move {
        let h = PlaneHarness::spawn(&url, plane_config(RateLimits::default())).await;
        let token = h.signup_token("kenny@example.com", "kenny").await;

        let resp = h.complete("signup", &token).await;
        assert_eq!(resp.status(), StatusCode::FOUND, "first click provisions");

        let resp = h.complete("signup", &token).await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "second click must be refused"
        );
        let body: Value = resp.json().await.expect("denial json");
        assert_eq!(body["error"], "invalid_link");

        assert_eq!(
            h.account_count("kenny@example.com").await,
            1,
            "no duplicate account"
        );
        let mappings: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM account_databases")
            .fetch_one(h.control.pool())
            .await
            .expect("count mappings");
        assert_eq!(mappings, 1, "no duplicate tenant database mapping");

        h.stop().await;
    })
    .await;
}

/// Dead tokens — missing, unknown, and expired — all get the one honest
/// `invalid_link` 400 with byte-identical bodies (no oracle over the
/// magic-link table), and none of them creates an account.
#[actix_web::test]
async fn dead_signup_links_are_one_honest_400() {
    with_control_db("dead_signup_links_are_one_honest_400", |url| async move {
        let h = PlaneHarness::spawn(&url, plane_config(RateLimits::default())).await;

        // Expired: a real link pushed past its expiry by direct SQL.
        let expired = h.signup_token("exp@example.com", "expired-slug").await;
        sqlx::query(
            "UPDATE magic_links SET expires_at = NOW() - INTERVAL '1 minute' \
             WHERE token_hash = $1",
        )
        .bind(sha256_hex(&expired))
        .execute(h.control.pool())
        .await
        .expect("expire link");

        let mut bodies = Vec::new();
        for (label, path) in [
            ("missing token", "/signup/complete".to_string()),
            (
                "unknown token",
                "/signup/complete?token=aml_nonsense".to_string(),
            ),
            ("expired token", format!("/signup/complete?token={expired}")),
        ] {
            let resp = h
                .on_host(Method::GET, &format!("app.{BASE_DOMAIN}"), &path)
                .send()
                .await
                .expect("send complete");
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{label} must 400");
            bodies.push(resp.bytes().await.expect("body"));
        }
        assert!(
            bodies.windows(2).all(|w| w[0] == w[1]),
            "all dead-token refusals must be byte-identical"
        );

        assert!(
            !h.link_consumed(&expired).await,
            "a refused consumption must not stamp consumed_at"
        );
        assert_eq!(
            h.account_count("exp@example.com").await,
            0,
            "no account may be created from a dead link"
        );

        h.stop().await;
    })
    .await;
}

/// Purpose crossover: a signup link on `/login/complete` (and a login link
/// on `/signup/complete`) is refused as `invalid_link` — and because the
/// purpose pin lives inside the consume UPDATE's WHERE clause, the
/// wrong-endpoint click does NOT burn the link: both still complete on
/// their own endpoints afterwards.
#[actix_web::test]
async fn completion_purpose_crossover_rejected() {
    with_control_db("completion_purpose_crossover_rejected", |url| async move {
        let h = PlaneHarness::spawn(&url, plane_config(RateLimits::default())).await;
        provision_account(
            &h.control,
            &h.cluster,
            &ManagedKeys::Disabled,
            NewAccount {
                email: "alpha@example.com".to_string(),
                subdomain: "alpha".to_string(),
            },
        )
        .await
        .expect("provision alpha");

        let signup_token = h.signup_token("new@example.com", "fresh").await;
        let login_token = h.login_token("alpha@example.com").await;

        for (kind, token) in [("login", &signup_token), ("signup", &login_token)] {
            let resp = h.complete(kind, token).await;
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "a {kind} completion must refuse the other purpose's link"
            );
            let body: Value = resp.json().await.expect("denial json");
            assert_eq!(body["error"], "invalid_link");
        }
        assert!(
            !h.link_consumed(&signup_token).await && !h.link_consumed(&login_token).await,
            "wrong-endpoint clicks must not burn the links"
        );

        // Both links still work where they belong.
        let resp = h.complete("signup", &signup_token).await;
        assert_eq!(resp.status(), StatusCode::FOUND);
        let resp = h.complete("login", &login_token).await;
        assert_eq!(resp.status(), StatusCode::FOUND);

        h.stop().await;
    })
    .await;
}

/// SubdomainTaken at consume time: two pending signup links for the same
/// slug, first click wins, second gets a structured 409 telling the user to
/// restart signup — with no orphan account rows. The second token stays
/// spent (module docs trade-off: re-requesting is cheap and rate-limited;
/// un-consuming would reopen the replay window).
#[actix_web::test]
async fn subdomain_taken_at_consume_is_conflict() {
    with_control_db("subdomain_taken_at_consume_is_conflict", |url| async move {
        let h = PlaneHarness::spawn(&url, plane_config(RateLimits::default())).await;

        // Both links issue fine: a link is a request, not a reservation.
        let first = h.signup_token("first@example.com", "shared").await;
        let second = h.signup_token("second@example.com", "shared").await;

        let resp = h.complete("signup", &first).await;
        assert_eq!(resp.status(), StatusCode::FOUND, "first consumer wins");

        let resp = h.complete("signup", &second).await;
        assert_eq!(
            resp.status(),
            StatusCode::CONFLICT,
            "the loser gets a structured 409"
        );
        let body: Value = resp.json().await.expect("conflict json");
        assert_eq!(body["error"], "subdomain_taken");
        assert!(
            body["message"]
                .as_str()
                .expect("message")
                .contains("Start signup again"),
            "the 409 must tell the user to restart signup"
        );

        // The losing token is spent, and nothing was orphaned: one account
        // owns the slug, the loser has no rows in any state.
        assert!(h.link_consumed(&second).await, "the losing token is spent");
        assert_eq!(h.account_count("second@example.com").await, 0);
        let provisioning: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM accounts WHERE status <> 'active'")
                .fetch_one(h.control.pool())
                .await
                .expect("count non-active accounts");
        assert_eq!(provisioning, 0, "no account row may be left mid-provision");
        let owners: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM accounts WHERE subdomain = 'shared'")
                .fetch_one(h.control.pool())
                .await
                .expect("count owners");
        assert_eq!(owners, 1);

        h.stop().await;
    })
    .await;
}

/// Saturation refuses BEFORE consuming: with the provision cap at 1 and the
/// only permit held (standing in for a slow in-flight provision), a
/// completion gets a structured 503 + Retry-After, its token stays
/// unconsumed, and the same link succeeds once capacity frees up.
#[actix_web::test]
async fn saturated_provisioning_refuses_without_consuming() {
    with_control_db(
        "saturated_provisioning_refuses_without_consuming",
        |url| async move {
            let h = PlaneHarness::spawn(
                &url,
                AccountPlaneConfig {
                    max_concurrent_provisions: 1,
                    ..AccountPlaneConfig::new(BASE_DOMAIN)
                },
            )
            .await;
            let token = h.signup_token("kenny@example.com", "kenny").await;

            let held = h
                .plane
                .provision_permits()
                .try_acquire_owned()
                .expect("hold the only provision permit");

            let resp = h.complete("signup", &token).await;
            assert_eq!(
                resp.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "a saturated process must refuse"
            );
            assert!(
                resp.headers().get(RETRY_AFTER).is_some(),
                "the 503 carries Retry-After"
            );
            let body: Value = resp.json().await.expect("denial json");
            assert_eq!(body["error"], "provisioning_busy");
            assert!(body["retry_after_seconds"].as_u64().unwrap_or(0) >= 1);

            // The refusal happened before consumption: the token is live
            // and no account was created.
            assert!(
                !h.link_consumed(&token).await,
                "saturation must not consume the token"
            );
            assert_eq!(h.account_count("kenny@example.com").await, 0);

            // Dead tokens are refused by the pre-permit checks even while
            // saturated — 400 invalid_link, never 503 — pinning that the
            // syntactic gate (malformed) and the read-only peek
            // (well-shaped but unknown) both run before the permit.
            for junk in [
                "definitely-not-a-token".to_string(),
                format!("aml_{}", "a".repeat(52)),
            ] {
                let resp = h.complete("signup", &junk).await;
                assert_eq!(
                    resp.status(),
                    StatusCode::BAD_REQUEST,
                    "{junk:?} must be refused as invalid even under saturation"
                );
                let body: Value = resp.json().await.expect("denial json");
                assert_eq!(body["error"], "invalid_link");
            }

            // Capacity frees up; the SAME link completes.
            drop(held);
            let resp = h.complete("signup", &token).await;
            assert_eq!(
                resp.status(),
                StatusCode::FOUND,
                "the token must remain usable after a saturation refusal"
            );

            h.stop().await;
        },
    )
    .await;
}

/// Permit-starvation regression: garbage completions must never hold a
/// provision permit. With the cap at 1, a legitimate signup completes
/// successfully on its first try WHILE a hammer of well-shaped-but-unknown
/// tokens runs — if junk acquired the only permit even briefly, the legit
/// request would race into 503s; and every hammered request must itself be
/// a 400, never a 503.
#[actix_web::test]
async fn garbage_completions_never_hold_permits() {
    with_control_db("garbage_completions_never_hold_permits", |url| async move {
        let h = PlaneHarness::spawn(
            &url,
            AccountPlaneConfig {
                max_concurrent_provisions: 1,
                ..AccountPlaneConfig::new(BASE_DOMAIN)
            },
        )
        .await;
        let token = h.signup_token("kenny@example.com", "kenny").await;

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hammer = {
            let stop = Arc::clone(&stop);
            let client = h.client.clone();
            let url = format!(
                "{}/signup/complete?token=aml_{}",
                h.base_url,
                "a".repeat(52)
            );
            actix_web::rt::spawn(async move {
                let mut refused = 0u64;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let resp = client
                        .get(&url)
                        .header(HOST, format!("app.{BASE_DOMAIN}"))
                        .send()
                        .await
                        .expect("send garbage complete");
                    assert_eq!(
                        resp.status(),
                        StatusCode::BAD_REQUEST,
                        "garbage must always be 400 — a 503 would mean it \
                         reached the permit"
                    );
                    refused += 1;
                }
                refused
            })
        };

        // The legit completion (a real multi-second provision) wins its
        // permit on the first try, mid-hammer.
        let resp = h.complete("signup", &token).await;
        assert_eq!(
            resp.status(),
            StatusCode::FOUND,
            "the legit completion must succeed while garbage hammers"
        );

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let refused = hammer.await.expect("hammer task");
        assert!(refused > 0, "the hammer must actually have run");

        h.stop().await;
    })
    .await;
}

/// Self-resume regression: a subdomain claimed by the requester's OWN stuck
/// `'provisioning'` account is not "taken" — re-requesting a link (in any
/// email casing) gets a 200 whose completion resumes the stuck provision to
/// active. A different email is still refused honestly.
#[actix_web::test]
async fn stuck_provision_resumes_via_fresh_request_link() {
    with_control_db(
        "stuck_provision_resumes_via_fresh_request_link",
        |url| async move {
            let h = PlaneHarness::spawn(&url, plane_config(RateLimits::default())).await;
            let app_host = format!("app.{BASE_DOMAIN}");

            // A crashed signup: the claim sits in 'provisioning' (nothing
            // else was done — the earliest possible crash point).
            let account_id = uuid::Uuid::new_v4();
            sqlx::query(
                "INSERT INTO accounts (id, subdomain, email, status, plan) \
                 VALUES ($1, 'stuck', 'kenny@example.com', 'provisioning', 'free')",
            )
            .bind(account_id.to_string())
            .execute(h.control.pool())
            .await
            .expect("seed stuck provision");

            // A different email is still honestly refused.
            let resp = h
                .request_signup_link(&app_host, "rival@example.com", "stuck")
                .await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            let body: Value = resp.json().await.expect("error json");
            assert_eq!(body["error"], "subdomain_taken");

            // The same email — in different casing, since every email
            // comparison is lowercased — gets a fresh link...
            let token = h.signup_token("Kenny@Example.com", "stuck").await;

            // ...whose completion RESUMES the stuck account (same id, now
            // active), rather than failing subdomain_taken.
            let resp = h.complete("signup", &token).await;
            assert_eq!(
                resp.status(),
                StatusCode::FOUND,
                "completion must resume the user's own stuck claim"
            );
            let status: String = sqlx::query_scalar("SELECT status FROM accounts WHERE id = $1")
                .bind(account_id.to_string())
                .fetch_one(h.control.pool())
                .await
                .expect("stuck account row still exists");
            assert_eq!(status, "active", "the original claim was resumed");
            let owners: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM accounts WHERE subdomain = 'stuck'")
                    .fetch_one(h.control.pool())
                    .await
                    .expect("count owners");
            assert_eq!(owners, 1, "resume, not a duplicate claim");

            h.stop().await;
        },
    )
    .await;
}

/// Every account-plane response carries `Referrer-Policy: no-referrer`:
/// completion URLs hold live single-use tokens, and neither the redirect
/// nor any error page may leak them onward via `Referer`.
#[actix_web::test]
async fn account_plane_responses_set_no_referrer() {
    with_control_db(
        "account_plane_responses_set_no_referrer",
        |url| async move {
            let h = PlaneHarness::spawn(&url, plane_config(RateLimits::default())).await;
            let app_host = format!("app.{BASE_DOMAIN}");

            fn assert_no_referrer(resp: &reqwest::Response, what: &str) {
                assert_eq!(
                    resp.headers()
                        .get("referrer-policy")
                        .and_then(|v| v.to_str().ok()),
                    Some("no-referrer"),
                    "{what} must carry Referrer-Policy: no-referrer"
                );
            }

            // Request-link successes and validation failures, both routes.
            let resp = h
                .request_signup_link(&app_host, "kenny@example.com", "kenny")
                .await;
            assert_eq!(resp.status(), StatusCode::OK);
            assert_no_referrer(&resp, "signup request-link 200");
            let resp = h.request_signup_link(&app_host, "garbage", "kenny").await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            assert_no_referrer(&resp, "signup request-link 400");
            let resp = h.request_login_link("kenny@example.com").await;
            assert_no_referrer(&resp, "login request-link 200");

            // Completion: the redirect (the URL that carried the token) and the
            // dead-token refusal.
            let sent = h.sender.sent();
            let token = token_from_link(&sent[0].link).to_string();
            let resp = h.complete("signup", &token).await;
            assert_eq!(resp.status(), StatusCode::FOUND);
            assert_no_referrer(&resp, "signup completion redirect");
            let resp = h.complete("signup", &token).await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            assert_no_referrer(&resp, "spent-token refusal");

            h.stop().await;
        },
    )
    .await;
}

// ==================== Login completion ====================

/// The full login flow over real HTTP: request a link for an existing
/// account, complete it, get the session cookie and the redirect to the
/// account's subdomain, and authenticate an API call with the cookie.
#[actix_web::test]
async fn login_complete_end_to_end() {
    with_control_db("login_complete_end_to_end", |url| async move {
        let h = PlaneHarness::spawn(&url, plane_config(RateLimits::default())).await;
        provision_account(
            &h.control,
            &h.cluster,
            &ManagedKeys::Disabled,
            NewAccount {
                email: "alpha@example.com".to_string(),
                subdomain: "alpha".to_string(),
            },
        )
        .await
        .expect("provision alpha");

        let token = h.login_token("alpha@example.com").await;
        let resp = h.complete("login", &token).await;
        assert_eq!(resp.status(), StatusCode::FOUND);
        assert_eq!(
            resp.headers()
                .get(LOCATION)
                .expect("302 carries Location")
                .to_str()
                .expect("ascii"),
            &format!("https://alpha.{BASE_DOMAIN}/"),
            "login redirects to the account's subdomain"
        );
        let cookie = set_cookie_header(&resp);
        assert_session_cookie_attributes(&cookie);
        let session = cookie_session_value(&cookie).to_string();
        assert!(h.link_consumed(&token).await);

        let resp = h
            .on_host(Method::GET, &format!("alpha.{BASE_DOMAIN}"), "/api/atoms")
            .header("Cookie", format!("{SESSION_COOKIE}={session}"))
            .send()
            .await
            .expect("send authenticated call");
        assert_eq!(resp.status(), StatusCode::OK);

        h.stop().await;
    })
    .await;
}

/// Chokepoint regression, cookie edition (plan decision 2026-06-09): the
/// `.{base}` cookie crosses subdomains by design, so a session minted by
/// the real login flow presented on the WRONG account's subdomain must
/// still 401 — CloudAuth's account-scoped verification, not the cookie
/// scope, is what isolates tenants. Slice 1 pinned hand-created sessions;
/// this pins the flow-issued cookie.
#[actix_web::test]
async fn flow_issued_cookie_rejected_on_other_tenants_subdomain() {
    with_control_db(
        "flow_issued_cookie_rejected_on_other_tenants_subdomain",
        |url| async move {
            let h = PlaneHarness::spawn(&url, plane_config(RateLimits::default())).await;
            for (email, subdomain) in [
                ("alpha@example.com", "alpha"),
                ("bravo@example.com", "bravo"),
            ] {
                provision_account(
                    &h.control,
                    &h.cluster,
                    &ManagedKeys::Disabled,
                    NewAccount {
                        email: email.to_string(),
                        subdomain: subdomain.to_string(),
                    },
                )
                .await
                .expect("provision account");
            }

            let token = h.login_token("alpha@example.com").await;
            let resp = h.complete("login", &token).await;
            assert_eq!(resp.status(), StatusCode::FOUND);
            let session = cookie_session_value(&set_cookie_header(&resp)).to_string();

            // Alpha's flow-issued cookie on bravo's subdomain → 401.
            let resp = h
                .on_host(Method::GET, &format!("bravo.{BASE_DOMAIN}"), "/api/atoms")
                .header("Cookie", format!("{SESSION_COOKIE}={session}"))
                .send()
                .await
                .expect("send cross-tenant call");
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "a session cookie must not cross tenants"
            );

            // …and still works where it belongs.
            let resp = h
                .on_host(Method::GET, &format!("alpha.{BASE_DOMAIN}"), "/api/atoms")
                .header("Cookie", format!("{SESSION_COOKIE}={session}"))
                .send()
                .await
                .expect("send same-tenant call");
            assert_eq!(resp.status(), StatusCode::OK);

            h.stop().await;
        },
    )
    .await;
}
