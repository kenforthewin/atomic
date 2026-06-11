//! End-to-end tests for the app-host account plane: the host-based plane
//! split (both fail-closed directions), signup/login request-link behavior,
//! login indistinguishability, and the anti-abuse rate limits.
//!
//! Each test spawns the real composition — `configure_cloud_app` on an
//! ephemeral port, exactly as `atomic-cloud serve` wires it — with a
//! capturing email sender (NO REAL EMAIL, EVER) and drives it with explicit
//! `Host` headers: `cloudtest.local` / `app.cloudtest.local` for the
//! account plane, `<subdomain>.cloudtest.local` for tenants.
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command.

mod support;

use std::sync::Arc;
use std::time::Duration;

use actix_web::{App, HttpServer};
use atomic_cloud::{
    configure_cloud_app, issue_token, provision_account, AccountCache, AccountCacheConfig,
    AccountPlane, AccountPlaneConfig, CloudAuth, ClusterConfig, ControlPlane, FallbackAppState,
    MagicLinkPurpose, NewAccount, RateLimits, TokenScope,
};
use reqwest::header::{HOST, RETRY_AFTER};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use support::{control_db_contains, with_control_db, CapturingSender, SentEmail};

/// Base domain the composition is configured with. The app host is this
/// name itself and `app.<BASE_DOMAIN>`.
const BASE_DOMAIN: &str = "cloudtest.local";

fn sha256_hex(plaintext: &str) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(plaintext.as_bytes()))
}

/// The composed cloud server on an ephemeral port, with the account plane
/// backed by a capturing sender.
struct PlaneHarness {
    control: ControlPlane,
    cluster: ClusterConfig,
    sender: CapturingSender,
    client: reqwest::Client,
    base_url: String,
    handle: actix_web::dev::ServerHandle,
    /// Owns the scratch directory behind the inert fallback `AppState`;
    /// must outlive the server.
    _fallback: FallbackAppState,
}

impl PlaneHarness {
    async fn spawn(control_url: &str, rate_limits: RateLimits) -> Self {
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
            AccountCacheConfig::default(),
        ));
        let auth = CloudAuth::new(control.clone(), Arc::clone(&cache), BASE_DOMAIN);
        let sender = CapturingSender::default();
        let account_plane = AccountPlane::new(
            control.clone(),
            Arc::new(sender.clone()),
            AccountPlaneConfig {
                rate_limits,
                ..AccountPlaneConfig::new(BASE_DOMAIN)
            },
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

        PlaneHarness {
            control,
            cluster,
            sender,
            client: reqwest::Client::new(),
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
}

/// Pull the `token=` value out of a captured link.
fn token_from_link(link: &str) -> &str {
    link.split("token=").nth(1).expect("link carries a token")
}

// ==================== Plane split ====================

/// Fail-closed direction one: the app host (bare base domain AND
/// `app.<base>`) must 404 every tenant route, even with a perfectly valid
/// tenant credential attached.
#[actix_web::test]
async fn app_host_404s_tenant_routes() {
    with_control_db("app_host_404s_tenant_routes", |url| async move {
        let h = PlaneHarness::spawn(&url, RateLimits::default()).await;
        let account = provision_account(
            &h.control,
            &h.cluster,
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
            let h = PlaneHarness::spawn(&url, RateLimits::default()).await;
            let account = provision_account(
                &h.control,
                &h.cluster,
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
        let h = PlaneHarness::spawn(&url, RateLimits::default()).await;

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
                RateLimits {
                    signup_links_per_ip: 100,
                    links_per_email: 100,
                    ..RateLimits::default()
                },
            )
            .await;
            // An existing account and an active reservation to collide with.
            provision_account(
                &h.control,
                &h.cluster,
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
            let h = PlaneHarness::spawn(&url, RateLimits::default()).await;
            provision_account(
                &h.control,
                &h.cluster,
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
            // pointing at the login completion route.
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

// ==================== Rate limits ====================

/// The per-IP signup limit admits exactly `limit` requests, refuses the
/// next with 429 + Retry-After, and admits again once the window passes.
/// Validation-failing requests count as attempts (they're charged before
/// validation), pinned by spending one slot on a bad slug.
#[actix_web::test]
async fn signup_ip_rate_limit_enforces_and_resets() {
    with_control_db(
        "signup_ip_rate_limit_enforces_and_resets",
        |url| async move {
            let window = Duration::from_millis(1500);
            let h = PlaneHarness::spawn(
                &url,
                RateLimits {
                    signup_links_per_ip: 3,
                    signup_ip_window: window,
                    // Distinct emails below keep the email limit out of play.
                    ..RateLimits::default()
                },
            )
            .await;
            let app_host = format!("app.{BASE_DOMAIN}");

            // Admission 1 — a validation failure still burns a slot.
            let resp = h
                .request_signup_link(&app_host, "a@example.com", "ab")
                .await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            // Admissions 2 and 3.
            for (email, slug) in [("b@example.com", "slug-b"), ("c@example.com", "slug-c")] {
                let resp = h.request_signup_link(&app_host, email, slug).await;
                assert_eq!(resp.status(), StatusCode::OK);
            }

            // Over the limit: 429 with Retry-After.
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
            assert_eq!(h.sender.sent().len(), 2, "the refused request sent nothing");

            // After the window the limiter resets.
            tokio::time::sleep(window + Duration::from_millis(200)).await;
            let resp = h
                .request_signup_link(&app_host, "d@example.com", "slug-d")
                .await;
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "limit must reset once the window passes"
            );

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
            RateLimits {
                links_per_email: 2,
                email_window: window,
                ..RateLimits::default()
            },
        )
        .await;
        provision_account(
            &h.control,
            &h.cluster,
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
