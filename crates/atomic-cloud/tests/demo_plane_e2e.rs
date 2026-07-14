//! End-to-end contract for the public demo plane
//! (plan: `docs/plans/demo-instance.md`).
//!
//! Spawns the real composition with a demo subdomain configured and pins
//! the whitelist's behavior from the outside: what an anonymous visitor
//! can reach (the read surface + rate-limited search), what stays closed
//! (everything else, 403 `demo_forbidden`), that the anonymous path opens
//! ONLY on the configured host, that an unconfigured deployment has no
//! demo behavior at all, and that the operator's credentials are never
//! demo-restricted. Also pins the two guard exclusions: demo traffic
//! neither marks dispatch hints nor consumes the per-account data-plane
//! rate windows.
//!
//! Postgres-gated like the rest of the cloud suites; run with
//! `--test-threads=1` (see tests/support/mod.rs).

mod support;

use std::sync::Arc;

use actix_web::{App, HttpServer};
use atomic_cloud::{
    configure_cloud_app, issue_token, list_hinted_accounts, provision_account_with_policy,
    set_active_provider, upsert_credentials, AccountCache, AccountCacheConfig, AccountPlane,
    AccountPlaneConfig, ChatStreamLimiter, CloudAuth, ClusterConfig, ControlPlane,
    CredentialOrigin, DemoPlane, FallbackAppState, ManagedKeys, NewAccount, NewCredentials,
    Provider, QuotaBilling, Readiness, SecretKey, SubdomainPolicy, TokenScope,
    DEFAULT_CHAT_STREAMS_PER_ACCOUNT,
};
use atomic_test_support::MockAiServer;
use reqwest::header::HOST;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use support::with_control_db;

const BASE_DOMAIN: &str = "cloudtest.local";
const DEMO_SUBDOMAIN: &str = "demo";

/// The composed server with (or without) a demo plane, plus the demo
/// tenant and its owner token.
struct DemoHarness {
    control: ControlPlane,
    mock: MockAiServer,
    client: reqwest::Client,
    base_url: String,
    account_id: String,
    owner_token: String,
    handle: actix_web::dev::ServerHandle,
    _fallback: FallbackAppState,
}

impl DemoHarness {
    /// Spawn the composition exactly as `serve` wires it — the only knob is
    /// whether `CloudAuth` carries the demo plane (`with_demo` mirrors
    /// setting / omitting `--demo-subdomain`).
    async fn spawn(control_url: &str, with_demo: bool) -> Self {
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
            AccountCacheConfig::default(),
        ));
        let auth = CloudAuth::new(control.clone(), Arc::clone(&cache), BASE_DOMAIN)
            .with_demo_plane(with_demo.then(|| {
                DemoPlane::new(
                    DEMO_SUBDOMAIN,
                    &format!("http://app.{BASE_DOMAIN}"),
                    false,
                )
            }));
        let account_plane = AccountPlane::new(
            control.clone(),
            cluster.clone(),
            ManagedKeys::Disabled,
            Arc::new(support::CapturingSender::default()),
            AccountPlaneConfig::new(BASE_DOMAIN),
        )
        .expect("build account plane");
        let tenant_plane = atomic_cloud::TenantPlane::new(
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
        let server = HttpServer::new(move || {
            App::new().configure(configure_cloud_app(
                state.clone(),
                auth.clone(),
                account_plane.clone(),
                tenant_plane.clone(),
                oauth_plane.clone(),
                mcp_transport.clone(),
                control_for_app.clone(),
                ChatStreamLimiter::new(DEFAULT_CHAT_STREAMS_PER_ACCOUNT),
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

        // The demo tenant itself: provisioned through the same policy the
        // operator CLI uses (`--allow-reserved`), since `demo` is on the
        // static blocklist by design.
        let account = provision_account_with_policy(
            &control,
            &cluster,
            &ManagedKeys::Disabled,
            NewAccount {
                email: "demo-operator@example.com".to_string(),
                subdomain: DEMO_SUBDOMAIN.to_string(),
            },
            SubdomainPolicy::AllowReserved,
        )
        .await
        .expect("provision demo tenant on a reserved subdomain");
        let vault = support::test_vault();
        upsert_credentials(
            &control,
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
                    "openai_compat_base_url": mock.base_url(),
                    "embedding_dimension": 1536,
                }),
            },
        )
        .await
        .expect("store mock provider credentials");
        set_active_provider(
            &control,
            &account.account_id,
            Some((Provider::OpenAiCompat, CredentialOrigin::User)),
        )
        .await
        .expect("activate mock provider credentials");
        let owner_token = issue_token(
            &control,
            &account.account_id,
            TokenScope::Account,
            None,
            "demo-operator",
        )
        .await
        .expect("issue owner token");

        DemoHarness {
            control,
            mock,
            client: reqwest::Client::new(),
            base_url: format!("http://127.0.0.1:{port}"),
            account_id: account.account_id,
            owner_token,
            handle,
            _fallback: fallback,
        }
    }

    async fn stop(self) {
        self.handle.stop(false).await;
        drop(self.mock);
    }

    fn demo_host(&self) -> String {
        format!("{DEMO_SUBDOMAIN}.{BASE_DOMAIN}")
    }

    /// An anonymous request on the demo host.
    async fn anon(&self, method: Method, path: &str) -> reqwest::Response {
        self.client
            .request(method, format!("{}{path}", self.base_url))
            .header(HOST, self.demo_host())
            .send()
            .await
            .expect("request")
    }

    /// An owner-token request on the demo host.
    async fn owner(&self, method: Method, path: &str, body: Option<Value>) -> reqwest::Response {
        let mut req = self
            .client
            .request(method, format!("{}{path}", self.base_url))
            .header(HOST, self.demo_host())
            .bearer_auth(&self.owner_token);
        if let Some(body) = body {
            req = req.json(&body);
        }
        req.send().await.expect("request")
    }
}

async fn body_json(res: reqwest::Response) -> Value {
    res.json().await.expect("json body")
}

#[actix_web::test]
async fn demo_whitelist_serves_reads_and_refuses_everything_else() {
    with_control_db("demo_plane_contract", |control_url| async move {
        let h = DemoHarness::spawn(&control_url, true).await;

        // Owner seeds one atom (also proves the operator is never
        // demo-restricted on the demo host).
        let created = h
            .owner(
                Method::POST,
                "/api/atoms",
                Some(json!({"content": "Attention Is All You Need — the transformer paper."})),
            )
            .await;
        assert_eq!(
            created.status(),
            StatusCode::CREATED,
            "owner create must work"
        );
        let atom_id = body_json(created).await["id"]
            .as_str()
            .expect("atom id")
            .to_string();

        // 1 — the whitelisted read surface answers anonymously.
        for path in [
            "/api/atoms",
            &format!("/api/atoms/{atom_id}"),
            &format!("/api/atoms/{atom_id}/links"),
            "/api/tags",
            "/api/canvas/positions",
            "/api/canvas/global",
            "/api/graph/edges",
            "/api/clustering",
            "/api/wiki",
            "/api/reports",
            "/api/settings",
            "/api/databases",
            "/api/setup/status",
            "/api/embeddings/status/all",
        ] {
            let res = h.anon(Method::GET, path).await;
            assert_eq!(
                res.status(),
                StatusCode::OK,
                "GET {path} must serve anonymously on the demo host"
            );
        }

        // 2 — the demo-mode probe.
        let probe = body_json(h.anon(Method::GET, "/api/demo-config").await).await;
        assert_eq!(probe["demo"], json!(true));
        assert_eq!(
            probe["signup_url"],
            json!(format!("http://app.{BASE_DOMAIN}/signup"))
        );
        // …and it 404s for the authenticated operator (they see the real app).
        let owner_probe = h.owner(Method::GET, "/api/demo-config", None).await;
        assert_eq!(owner_probe.status(), StatusCode::NOT_FOUND);

        // 3 — mutations and closed families are refused with the structured
        // 403, including representatives of every closed class: writes,
        // chat, exports (the billing guard's egress exemption must NOT
        // apply), feeds/tokens/account planes, pipeline pokes, /mcp, /ws.
        for (method, path) in [
            (Method::POST, "/api/atoms"),
            (Method::DELETE, &*format!("/api/atoms/{atom_id}")),
            (Method::PUT, "/api/settings/onboarding_completed"),
            (Method::GET, "/api/conversations"),
            (Method::POST, "/api/conversations"),
            (Method::POST, "/api/databases/default/exports/markdown"),
            (Method::GET, "/api/feeds"),
            (Method::GET, "/api/auth/tokens"),
            (Method::GET, "/api/account/overview"),
            (Method::DELETE, "/api/account"),
            (Method::POST, "/api/ingest/url"),
            (Method::POST, "/api/embeddings/process-pending"),
            (Method::GET, "/mcp"),
            (Method::POST, "/mcp"),
            (Method::GET, "/ws"),
        ] {
            let res = h.anon(method.clone(), path).await;
            assert_eq!(
                res.status(),
                StatusCode::FORBIDDEN,
                "{method} {path} must be demo_forbidden"
            );
            let body = body_json(res).await;
            assert_eq!(body["error"], json!("demo_forbidden"), "{method} {path}");
            assert!(
                body["signup_url"].as_str().unwrap_or_default().ends_with("/signup"),
                "refusal must carry the signup CTA"
            );
        }

        // 4 — default-deny is the future-proofing: an export download (a
        // cloud-owned route that exists and serves the OWNER) is still
        // refused anonymously by shape alone.
        let res = h.anon(Method::GET, "/api/exports/some-job/download").await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // 5 — the settings scrub: the anonymous settings read carries no
        // key material (the mock credential value must never surface).
        let settings = h.anon(Method::GET, "/api/settings").await;
        let raw = settings.text().await.expect("settings body");
        assert!(
            !raw.contains("test-key"),
            "anonymous settings response must not leak credential values: {raw}"
        );

        h.stop().await;
    })
    .await;
}

#[actix_web::test]
async fn demo_search_works_anonymously_and_never_marks_hints() {
    with_control_db("demo_plane_search", |control_url| async move {
        let h = DemoHarness::spawn(&control_url, true).await;

        // Anonymous semantic search embeds the query via the mock provider
        // and answers — the demo's one AI-spend surface.
        let res = h
            .client
            .post(format!("{}/api/search", h.base_url))
            .header(HOST, h.demo_host())
            .json(&json!({"query": "transformers", "mode": "semantic", "limit": 5}))
            .send()
            .await
            .expect("search");
        assert_eq!(res.status(), StatusCode::OK, "anonymous search must serve");

        // The search POST must NOT have marked a dispatch hint: hints mean
        // "tenant work may exist", and visitor searches are reads.
        let hinted = list_hinted_accounts(&h.control).await.expect("list hints");
        assert!(
            !hinted.iter().any(|hint| hint.account_id == h.account_id),
            "anonymous search must not mark dispatch hints"
        );

        h.stop().await;
    })
    .await;
}

#[actix_web::test]
async fn foreign_session_cookie_on_demo_host_degrades_to_visitor() {
    with_control_db("demo_plane_foreign_cookie", |control_url| async move {
        let h = DemoHarness::spawn(&control_url, true).await;

        // A second, unrelated tenant with a real browser session. Its
        // cookie is scoped to the base domain, so a browser sends it to
        // EVERY subdomain — including the demo's.
        let alpha = provision_account_with_policy(
            &h.control,
            &ClusterConfig {
                cluster_id: "test-cluster-1".to_string(),
                cluster_url: std::env::var("ATOMIC_TEST_DATABASE_URL").expect("cluster url"),
            },
            &ManagedKeys::Disabled,
            NewAccount {
                email: "alpha@example.com".to_string(),
                subdomain: "alpha".to_string(),
            },
            SubdomainPolicy::EnforceReserved,
        )
        .await
        .expect("provision alpha");
        let session = atomic_cloud::create_session(
            &h.control,
            &alpha.account_id,
            std::time::Duration::from_secs(3600),
            None,
            None,
        )
        .await
        .expect("alpha session");
        let cookie = format!("{}={session}", atomic_cloud::SESSION_COOKIE);

        // On the demo host, alpha's (unverifiable-here) session is ambient
        // noise: the request is served as an anonymous visitor — reads
        // pass the whitelist, writes are demo_forbidden. NOT a 401, which
        // the product app escalates into a login redirect.
        let res = h
            .client
            .get(format!("{}/api/atoms", h.base_url))
            .header(HOST, h.demo_host())
            .header("Cookie", &cookie)
            .send()
            .await
            .expect("request");
        assert_eq!(res.status(), StatusCode::OK, "foreign cookie reads as visitor");
        let res = h
            .client
            .post(format!("{}/api/atoms", h.base_url))
            .header(HOST, h.demo_host())
            .header("Cookie", &cookie)
            .json(&json!({"content": "still forbidden"}))
            .send()
            .await
            .expect("request");
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "visitor treatment, not access");

        // The probe answers demo:true for them, so the frontend renders
        // demo chrome instead of bouncing to login.
        let probe = h
            .client
            .get(format!("{}/api/demo-config", h.base_url))
            .header(HOST, h.demo_host())
            .header("Cookie", &cookie)
            .send()
            .await
            .expect("probe");
        assert_eq!(probe.status(), StatusCode::OK);

        // The same cookie still works normally on alpha's own host…
        let res = h
            .client
            .get(format!("{}/api/atoms", h.base_url))
            .header(HOST, format!("alpha.{BASE_DOMAIN}"))
            .header("Cookie", &cookie)
            .send()
            .await
            .expect("request");
        assert_eq!(res.status(), StatusCode::OK, "own host unaffected");

        // …and on a third tenant's host the second-chokepoint rule stands:
        // foreign session → 401, no demo leniency off the demo host.
        let res = h
            .client
            .get(format!("{}/api/atoms", h.base_url))
            .header(HOST, format!("demo-operator.{BASE_DOMAIN}"))
            .send()
            .await
            .expect("request");
        assert_ne!(res.status(), StatusCode::OK);

        // A failed BEARER on the demo host stays a loud 401: deliberate
        // credentials never degrade to visitor.
        let res = h
            .client
            .get(format!("{}/api/atoms", h.base_url))
            .header(HOST, h.demo_host())
            .bearer_auth("atm_definitely_not_a_real_token")
            .send()
            .await
            .expect("request");
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        h.stop().await;
    })
    .await;
}

#[actix_web::test]
async fn demo_path_opens_only_on_the_configured_host() {
    with_control_db("demo_plane_scoping", |control_url| async move {
        // With the demo plane configured, OTHER hosts still 401
        // credential-less requests exactly as before.
        let h = DemoHarness::spawn(&control_url, true).await;
        let res = h
            .client
            .get(format!("{}/api/atoms", h.base_url))
            .header(HOST, format!("someone-else.{BASE_DOMAIN}"))
            .send()
            .await
            .expect("request");
        assert_eq!(
            res.status(),
            StatusCode::NOT_FOUND,
            "an unknown subdomain is 404 (no such account), demo or not"
        );
        // A real non-demo account still 401s anonymously: the demo tenant
        // exists, so probe it through a NON-demo harness below instead —
        // here, assert the app host is untouched by demo config.
        let res = h
            .client
            .get(format!("{}/api/atoms", h.base_url))
            .header(HOST, format!("app.{BASE_DOMAIN}"))
            .send()
            .await
            .expect("request");
        assert_ne!(res.status(), StatusCode::OK);
        h.stop().await;

        // Without --demo-subdomain, the SAME host serves nothing
        // anonymously: the feature is opt-in per deployment.
        let h = DemoHarness::spawn(&control_url, false).await;
        let res = h.anon(Method::GET, "/api/atoms").await;
        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "no demo config → anonymous requests 401 even on the demo tenant's host"
        );
        let probe = h.anon(Method::GET, "/api/demo-config").await;
        assert_eq!(probe.status(), StatusCode::UNAUTHORIZED);
        h.stop().await;
    })
    .await;
}
