//! Plan-tier quota enforcement, the data-plane rate-limit rows, and the
//! dunning state machine — driven against the real composed cloud server
//! (plan: "Observability, quotas, billing" → "Quotas", "Billing").
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command. Run single-threaded:
//!
//! ```sh
//! CARGO_INCREMENTAL=0 \
//! ATOMIC_TEST_DATABASE_URL=postgres://atomic:atomic_test@localhost:5433/atomic_test \
//!   cargo test -p atomic-cloud --test quota_billing -- --test-threads=1
//! ```
//!
//! The plan-limit tests shrink the seeded `free` plan to a tiny limit (via
//! SQL, before the plan registry loads) so a handful of creates exercises
//! the boundary without minting a hundred atoms. The dunning tests drive the
//! time-machine by manufacturing a past `past_due_since` via SQL and calling
//! [`advance_dunning`] with the real clock — no real waits.

mod support;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use actix_web::{web, App, HttpServer};
use atomic_cloud::{
    advance_dunning, advance_expired_trials, apply_payment_failed, apply_payment_succeeded,
    apply_subscription_deleted, apply_subscription_event, apply_subscription_event_on_conn,
    claim_webhook_event, claim_webhook_event_on_conn, configure_cloud_app, expired_trials,
    finish_expired_trial, issue_token, link_stripe_customer, provision_account,
    release_webhook_event, start_trial, AccountCache, AccountCacheConfig, AccountPlane,
    AccountPlaneConfig, ChatStreamLimiter, CloudAuth, ClusterConfig, ControlPlane,
    DataPlaneRateLimiter, DataPlaneRateLimits, FallbackAppState, ManagedKeys, NewAccount,
    PlanRegistry, QuotaBilling, Readiness, SubscriptionState, TenantPlane, TokenScope,
    DEFAULT_CHAT_STREAMS_PER_ACCOUNT,
};
use reqwest::header::HOST;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use support::with_control_db;

const BASE_DOMAIN: &str = "cloudtest.local";

/// A provisioned tenant on the harness.
struct Tenant {
    account_id: String,
    subdomain: String,
    token: String,
}

/// The composed cloud server, parameterized so a test can shrink the `free`
/// plan's limits and the data-plane rate limits before the registry/limiter
/// are built.
struct Harness {
    control: ControlPlane,
    cluster: ClusterConfig,
    client: reqwest::Client,
    base_url: String,
    handle: actix_web::dev::ServerHandle,
    _fallback: FallbackAppState,
}

impl Harness {
    /// Spawn with billing disabled (no Stripe provider) — the default for the
    /// quota/rate-limit tests, which never exercise the webhook route.
    async fn spawn(control_url: &str, rate_limits: DataPlaneRateLimits) -> Self {
        let control = ControlPlane::connect(control_url)
            .await
            .expect("connect control plane");
        control.initialize().await.expect("migrate control plane");
        let billing = atomic_cloud::Billing::with_provider(
            control.clone(),
            None,
            "",
            HashMap::new(),
            format!("https://app.{BASE_DOMAIN}"),
            BASE_DOMAIN,
        );
        Self::spawn_with_billing(control, rate_limits, billing).await
    }

    /// Spawn with a fully-configured [`atomic_cloud::Billing`] (a scriptable
    /// provider + a real webhook secret), so the webhook route can verify a
    /// signed payload and drive the subscription lifecycle end-to-end.
    async fn spawn_with_billing(
        control: ControlPlane,
        rate_limits: DataPlaneRateLimits,
        billing: atomic_cloud::Billing,
    ) -> Self {
        let cluster = ClusterConfig {
            cluster_id: "test-cluster-1".to_string(),
            cluster_url: std::env::var("ATOMIC_TEST_DATABASE_URL")
                .expect("with_control_db verified ATOMIC_TEST_DATABASE_URL"),
        };

        // Build the plan registry AFTER any plan-limit mutation a test made
        // (tests mutate via `set_free_limits` before calling spawn).
        let plan_registry = web::Data::new(
            PlanRegistry::load(control.clone())
                .await
                .expect("load plans"),
        );

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
        let oauth_plane = atomic_cloud::OAuthPlane::new(
            control.clone(),
            BASE_DOMAIN,
            "http",
            format!("http://app.{BASE_DOMAIN}"),
        );
        let mcp_transport = fallback.mcp_transport(atomic_cloud::DEFAULT_MCP_SSE_KEEP_ALIVE);
        let control_for_app = control.clone();
        let chat_streams = ChatStreamLimiter::new(DEFAULT_CHAT_STREAMS_PER_ACCOUNT);
        let readiness = Readiness::ready(control.clone());
        let quota_billing = QuotaBilling {
            plan_registry,
            rate_limiter: DataPlaneRateLimiter::new(rate_limits),
            billing,
        };
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

        Harness {
            control,
            cluster,
            client: reqwest::Client::new(),
            base_url: format!("http://127.0.0.1:{port}"),
            handle,
            _fallback: fallback,
        }
    }

    async fn stop(self) {
        self.handle.stop(false).await;
    }

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
        let token = issue_token(
            &self.control,
            &account.account_id,
            TokenScope::Account,
            None,
            "quota-e2e",
        )
        .await
        .expect("issue token");
        Tenant {
            account_id: account.account_id,
            subdomain: subdomain.to_string(),
            token,
        }
    }

    fn api(&self, method: Method, subdomain: &str, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base_url))
            .header(HOST, format!("{subdomain}.{BASE_DOMAIN}"))
    }

    /// POST a single atom; returns the full response so the caller can assert
    /// status and body.
    async fn create_atom(&self, tenant: &Tenant, content: &str) -> reqwest::Response {
        self.api(Method::POST, &tenant.subdomain, "/api/atoms")
            .bearer_auth(&tenant.token)
            .json(&json!({ "content": content }))
            .send()
            .await
            .expect("send create atom")
    }
}

/// Shrink the seeded `free` plan's limits in this test's control database,
/// BEFORE the harness loads its plan registry. Per-test control DB, so this
/// only ever touches test data.
async fn set_free_limits(control_url: &str, atom_limit: Option<i32>, kb_limit: Option<i32>) {
    let control = ControlPlane::connect(control_url).await.expect("connect");
    control.initialize().await.expect("migrate");
    sqlx::query("UPDATE plans SET atom_limit = $1, kb_limit = $2 WHERE id = 'free'")
        .bind(atom_limit)
        .bind(kb_limit)
        .execute(control.pool())
        .await
        .expect("set free limits");
}

#[tokio::test]
async fn plans_are_seeded_and_account_plan_id_is_an_fk() {
    with_control_db("plans_seeded_and_fk", |url| async move {
        let control = ControlPlane::connect(&url).await.expect("connect");
        control.initialize().await.expect("migrate");

        // Migration 010 seeds at least 'free' and 'pro'.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM plans")
            .fetch_one(control.pool())
            .await
            .expect("count plans");
        assert!(count >= 2, "expected seeded free+pro plans, got {count}");

        let free: (Option<i32>, i32, Option<i32>) = sqlx::query_as(
            "SELECT atom_limit, ai_credits_monthly_cents, kb_limit FROM plans WHERE id = 'free'",
        )
        .fetch_one(control.pool())
        .await
        .expect("free plan row");
        assert_eq!(free, (Some(100), 50, Some(1)), "free-tier defaults");

        // The FK is real: an account can't reference a non-existent plan.
        let bad = sqlx::query(
            "INSERT INTO accounts (id, subdomain, email, status, plan, plan_id) \
             VALUES ('a-bad', 'fk-test', 'k@example.com', 'active', 'free', 'nonexistent')",
        )
        .execute(control.pool())
        .await;
        assert!(bad.is_err(), "plan_id FK must reject an unknown plan");

        // The registry loads and resolves the free plan.
        let registry = PlanRegistry::load(control.clone()).await.expect("load");
        let free = registry.get("free").expect("free plan present");
        assert_eq!(free.atom_limit, Some(100));
        assert_eq!(free.kb_limit, Some(1));
    })
    .await;
}

#[actix_web::test]
async fn provision_stamps_plan_id_free() {
    with_control_db("provision_stamps_free", |url| async move {
        let harness = Harness::spawn(&url, DataPlaneRateLimits::default()).await;
        let tenant = harness.provision("alpha").await;

        let plan_id =
            sqlx::query_scalar::<_, Option<String>>("SELECT plan_id FROM accounts WHERE id = $1")
                .bind(&tenant.account_id)
                .fetch_one(harness.control.pool())
                .await
                .expect("read plan_id");
        assert_eq!(plan_id.as_deref(), Some("free"), "provision stamps free");
        harness.stop().await;
    })
    .await;
}

#[actix_web::test]
async fn atom_limit_blocks_with_exact_quota_body() {
    with_control_db("atom_limit_enforced", |url| async move {
        // Shrink free to 2 atoms BEFORE the registry loads.
        set_free_limits(&url, Some(2), Some(1)).await;
        let harness = Harness::spawn(&url, DataPlaneRateLimits::default()).await;
        let tenant = harness.provision("alpha").await;

        // First two creates succeed (count 0→1, 1→2).
        for i in 0..2 {
            let resp = harness.create_atom(&tenant, &format!("atom {i}")).await;
            assert_eq!(resp.status(), StatusCode::CREATED, "create {i}");
        }

        // The third would land on count 3 > limit 2 → 402 with the exact
        // plan-specified body shape.
        let resp = harness.create_atom(&tenant, "atom 2").await;
        assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED, "over limit");
        let body: Value = resp.json().await.expect("quota body");
        assert_eq!(body["error"], "quota_exceeded");
        assert_eq!(body["metric"], "atoms");
        assert_eq!(body["current"], 2);
        assert_eq!(body["limit"], 2);
        assert!(body["resets_at"].is_null(), "resource limits don't reset");
        assert_eq!(
            body["upgrade_url"],
            format!("https://app.{BASE_DOMAIN}/billing")
        );

        harness.stop().await;
    })
    .await;
}

#[actix_web::test]
async fn unlimited_plan_never_blocks_atoms() {
    with_control_db("atom_limit_unlimited", |url| async move {
        // NULL atom_limit = unlimited.
        set_free_limits(&url, None, Some(1)).await;
        let harness = Harness::spawn(&url, DataPlaneRateLimits::default()).await;
        let tenant = harness.provision("alpha").await;

        // Many creates, none blocked (a finite limit would block well before).
        for i in 0..5 {
            let resp = harness.create_atom(&tenant, &format!("atom {i}")).await;
            assert_eq!(
                resp.status(),
                StatusCode::CREATED,
                "unlimited plan never blocks ({i})"
            );
        }
        harness.stop().await;
    })
    .await;
}

#[actix_web::test]
async fn bulk_create_respects_the_batch_delta() {
    with_control_db("bulk_batch_delta", |url| async move {
        // Free = 3 atoms.
        set_free_limits(&url, Some(3), Some(1)).await;
        let harness = Harness::spawn(&url, DataPlaneRateLimits::default()).await;
        let tenant = harness.provision("alpha").await;

        // A bulk of 5 against an empty (0) tenant: 0 + 5 > 3 → blocked as one
        // 402, no partial creation (the guard runs before the handler).
        let resp = harness
            .api(Method::POST, &tenant.subdomain, "/api/atoms/bulk")
            .bearer_auth(&tenant.token)
            .json(&json!([
                { "content": "a" }, { "content": "b" }, { "content": "c" },
                { "content": "d" }, { "content": "e" }
            ]))
            .send()
            .await
            .expect("send bulk");
        assert_eq!(
            resp.status(),
            StatusCode::PAYMENT_REQUIRED,
            "bulk over limit"
        );
        let body: Value = resp.json().await.expect("body");
        assert_eq!(body["metric"], "atoms");
        assert_eq!(body["current"], 0);

        // Nothing was created — a within-limit bulk of 2 now succeeds.
        let ok = harness
            .api(Method::POST, &tenant.subdomain, "/api/atoms/bulk")
            .bearer_auth(&tenant.token)
            .json(&json!([{ "content": "a" }, { "content": "b" }]))
            .send()
            .await
            .expect("send bulk 2");
        // The bulk handler returns 201 Created on success; the key assertion
        // is that the within-limit batch is NOT a 402 quota denial.
        assert!(
            ok.status().is_success(),
            "within-limit bulk succeeds, got {}",
            ok.status()
        );

        harness.stop().await;
    })
    .await;
}

#[actix_web::test]
async fn url_ingestion_counts_against_atom_limit() {
    with_control_db("ingest_atom_quota", |url| async move {
        // Free = 1 atom, so an account at its ceiling can't slip past the
        // quota gate by ingesting instead of creating.
        set_free_limits(&url, Some(1), Some(1)).await;
        let harness = Harness::spawn(&url, DataPlaneRateLimits::default()).await;
        let tenant = harness.provision("alpha").await;

        // Land exactly at the limit with a direct create (count 0→1).
        let first = harness.create_atom(&tenant, "atom 0").await;
        assert_eq!(first.status(), StatusCode::CREATED);

        // Single-URL ingestion (delta 1): 1 + 1 > 1 → 402 BEFORE the handler
        // ever fetches the URL. The body is the exact quota shape, proving the
        // ingest path enforces atom_limit, not just the 30/min rate limit.
        let single = harness
            .api(Method::POST, &tenant.subdomain, "/api/ingest/url")
            .bearer_auth(&tenant.token)
            .json(&json!({ "url": "https://example.com/x" }))
            .send()
            .await
            .expect("ingest one");
        assert_eq!(
            single.status(),
            StatusCode::PAYMENT_REQUIRED,
            "single ingest over the atom limit"
        );
        let body: Value = single.json().await.expect("body");
        assert_eq!(body["error"], "quota_exceeded");
        assert_eq!(body["metric"], "atoms");
        assert_eq!(body["current"], 1);
        assert_eq!(body["limit"], 1);

        // Batch ingestion delta is read from the `urls` field: 1 + 2 > 1 → 402
        // as a single denial, no partial fetch.
        let batch = harness
            .api(Method::POST, &tenant.subdomain, "/api/ingest/urls")
            .bearer_auth(&tenant.token)
            .json(&json!({ "urls": [
                { "url": "https://example.com/a" },
                { "url": "https://example.com/b" }
            ] }))
            .send()
            .await
            .expect("ingest batch");
        assert_eq!(
            batch.status(),
            StatusCode::PAYMENT_REQUIRED,
            "batch ingest over the atom limit"
        );
        let body: Value = batch.json().await.expect("batch body");
        assert_eq!(body["metric"], "atoms");

        harness.stop().await;
    })
    .await;
}

#[actix_web::test]
async fn kb_limit_blocks_database_create() {
    with_control_db("kb_limit_enforced", |url| async move {
        // Free = 1 KB (the default seed); a fresh tenant already has its
        // default KB, so any create is over the limit.
        let harness = Harness::spawn(&url, DataPlaneRateLimits::default()).await;
        let tenant = harness.provision("alpha").await;

        let resp = harness
            .api(Method::POST, &tenant.subdomain, "/api/databases")
            .bearer_auth(&tenant.token)
            .json(&json!({ "name": "Second KB" }))
            .send()
            .await
            .expect("send create db");
        assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED, "kb over limit");
        let body: Value = resp.json().await.expect("body");
        assert_eq!(body["error"], "quota_exceeded");
        assert_eq!(body["metric"], "knowledge_bases");
        assert_eq!(body["current"], 1);
        assert_eq!(body["limit"], 1);

        harness.stop().await;
    })
    .await;
}

#[actix_web::test]
async fn atom_limit_is_account_wide_across_knowledge_bases() {
    // The atom ceiling is an ACCOUNT limit, not per-KB: a tenant on a finite
    // atom plan with kb_limit > 1 must not evade it by spreading atoms across
    // knowledge bases (each KB under, the account over). The request-time gate
    // sums count_atoms across every KB, matching the sweep's semantics.
    with_control_db("atom_limit_account_wide", |url| async move {
        // Free = 3 atoms account-wide, up to 3 KBs.
        set_free_limits(&url, Some(3), Some(3)).await;
        let harness = Harness::spawn(&url, DataPlaneRateLimits::default()).await;
        let tenant = harness.provision("alpha").await;

        // Two atoms in the default KB (account total 2).
        for i in 0..2 {
            assert_eq!(
                harness
                    .create_atom(&tenant, &format!("default {i}"))
                    .await
                    .status(),
                StatusCode::CREATED,
                "default-KB create {i}"
            );
        }

        // Create a second KB and grab its id.
        let kb: Value = harness
            .api(Method::POST, &tenant.subdomain, "/api/databases")
            .bearer_auth(&tenant.token)
            .json(&json!({ "name": "Second KB" }))
            .send()
            .await
            .expect("create second kb")
            .json()
            .await
            .expect("kb body");
        let kb_id = kb["id"].as_str().expect("second kb id").to_string();

        // One atom in the second KB lands the ACCOUNT total at exactly the
        // limit (2 + 1 = 3). Targeted via the X-Atomic-Database header, exactly
        // as the handler resolves the KB.
        let in_kb2 = harness
            .api(Method::POST, &tenant.subdomain, "/api/atoms")
            .bearer_auth(&tenant.token)
            .header("X-Atomic-Database", &kb_id)
            .json(&json!({ "content": "kb2 atom" }))
            .send()
            .await
            .expect("create in kb2");
        assert_eq!(
            in_kb2.status(),
            StatusCode::CREATED,
            "third atom (in KB2) reaches the account-wide limit"
        );

        // A fourth atom in the SECOND KB would land the account at 4 > 3 — even
        // though KB2 holds only 1 atom. Per-KB counting would wrongly admit it;
        // account-wide counting blocks it.
        let over_in_kb2 = harness
            .api(Method::POST, &tenant.subdomain, "/api/atoms")
            .bearer_auth(&tenant.token)
            .header("X-Atomic-Database", &kb_id)
            .json(&json!({ "content": "kb2 over" }))
            .send()
            .await
            .expect("create over in kb2");
        assert_eq!(
            over_in_kb2.status(),
            StatusCode::PAYMENT_REQUIRED,
            "the account-wide ceiling blocks a spread-across-KB create"
        );
        let body: Value = over_in_kb2.json().await.expect("quota body");
        assert_eq!(body["error"], "quota_exceeded");
        assert_eq!(body["metric"], "atoms");
        assert_eq!(body["current"], 3, "account-wide sum across both KBs");
        assert_eq!(body["limit"], 3);

        // And the default KB is equally blocked (same account-wide count).
        assert_eq!(
            harness.create_atom(&tenant, "default over").await.status(),
            StatusCode::PAYMENT_REQUIRED,
            "the default KB sees the same account-wide ceiling"
        );

        harness.stop().await;
    })
    .await;
}

#[actix_web::test]
async fn atom_limit_is_isolated_across_tenants() {
    with_control_db("atom_limit_isolated", |url| async move {
        set_free_limits(&url, Some(1), Some(5)).await;
        let harness = Harness::spawn(&url, DataPlaneRateLimits::default()).await;
        let alpha = harness.provision("alpha").await;
        let other = harness.provision("tenanttwo").await;

        // alpha fills its single-atom budget.
        assert_eq!(
            harness.create_atom(&alpha, "a").await.status(),
            StatusCode::CREATED
        );
        assert_eq!(
            harness.create_atom(&alpha, "a2").await.status(),
            StatusCode::PAYMENT_REQUIRED,
            "alpha at its limit"
        );

        // the other tenant is unaffected — its own count is 0.
        assert_eq!(
            harness.create_atom(&other, "b").await.status(),
            StatusCode::CREATED,
            "the other tenant's gate is independent of alpha's count"
        );
        harness.stop().await;
    })
    .await;
}

#[actix_web::test]
async fn atom_create_rate_limit_is_per_account_and_resets() {
    with_control_db("rate_limit_atom_creates", |url| async move {
        // Tiny atom-create limit, generous everything else, short window so
        // the reset is observable without a long wait.
        let harness = Harness::spawn(
            &url,
            DataPlaneRateLimits {
                requests: 1000,
                atom_creates: 2,
                url_ingestion: 30,
                window: Duration::from_secs(1),
            },
        )
        .await;
        let alpha = harness.provision("alpha").await;
        let other = harness.provision("tenanttwo").await;

        // Two creates admit, the third 429s as the atom-create limit.
        assert_eq!(
            harness.create_atom(&alpha, "1").await.status(),
            StatusCode::CREATED
        );
        assert_eq!(
            harness.create_atom(&alpha, "2").await.status(),
            StatusCode::CREATED
        );
        let limited = harness.create_atom(&alpha, "3").await;
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(
            limited.headers().get("retry-after").is_some(),
            "429 carries Retry-After"
        );
        let body: Value = limited.json().await.expect("429 body");
        assert_eq!(body["error"], "rate_limited");
        assert_eq!(body["limit"], "atom_creates");

        // Another account is unaffected (per-account windows).
        assert_eq!(
            harness.create_atom(&other, "1").await.status(),
            StatusCode::CREATED,
            "the other tenant has its own atom-create budget"
        );

        // The window slides: after it elapses, alpha admits again.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(
            harness.create_atom(&alpha, "4").await.status(),
            StatusCode::CREATED,
            "window reset lets alpha create again"
        );
        harness.stop().await;
    })
    .await;
}

#[actix_web::test]
async fn broad_request_rate_limit_catches_any_route() {
    with_control_db("rate_limit_requests", |url| async move {
        // Request limit of 2; any third authenticated request 429s.
        let harness = Harness::spawn(
            &url,
            DataPlaneRateLimits {
                requests: 2,
                atom_creates: 1000,
                url_ingestion: 1000,
                window: Duration::from_secs(60),
            },
        )
        .await;
        let alpha = harness.provision("alpha").await;

        // Two GETs (reads) admit.
        for _ in 0..2 {
            let resp = harness
                .api(Method::GET, &alpha.subdomain, "/api/atoms")
                .bearer_auth(&alpha.token)
                .send()
                .await
                .expect("list");
            assert_eq!(resp.status(), StatusCode::OK);
        }
        // The third request of ANY type 429s on the broad request limit.
        let limited = harness
            .api(Method::GET, &alpha.subdomain, "/api/tags")
            .bearer_auth(&alpha.token)
            .send()
            .await
            .expect("third");
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        let body: Value = limited.json().await.expect("body");
        assert_eq!(body["limit"], "requests");
        harness.stop().await;
    })
    .await;
}

#[actix_web::test]
async fn url_ingestion_rate_limit_is_enforced() {
    with_control_db("rate_limit_url_ingestion", |url| async move {
        let harness = Harness::spawn(
            &url,
            DataPlaneRateLimits {
                requests: 1000,
                atom_creates: 1000,
                url_ingestion: 1,
                window: Duration::from_secs(60),
            },
        )
        .await;
        let alpha = harness.provision("alpha").await;

        // The first ingestion charges the narrow limit (the handler may
        // 4xx/5xx on the fake URL, but it is admitted past the guard); the
        // second is refused as url_ingestion before the handler.
        let first = harness
            .api(Method::POST, &alpha.subdomain, "/api/ingest/url")
            .bearer_auth(&alpha.token)
            .json(&json!({ "url": "https://example.com/x" }))
            .send()
            .await
            .expect("first ingest");
        assert_ne!(
            first.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "first ingestion is admitted"
        );

        let second = harness
            .api(Method::POST, &alpha.subdomain, "/api/ingest/url")
            .bearer_auth(&alpha.token)
            .json(&json!({ "url": "https://example.com/y" }))
            .send()
            .await
            .expect("second ingest");
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        let body: Value = second.json().await.expect("body");
        assert_eq!(body["limit"], "url_ingestion");
        harness.stop().await;
    })
    .await;
}

// --- Billing: subscription lifecycle + dunning state machine ----------------

#[tokio::test]
async fn subscription_lifecycle_moves_plan_and_clears_dunning() {
    with_control_db("subscription_lifecycle", |url| async move {
        let control = ControlPlane::connect(&url).await.expect("connect");
        control.initialize().await.expect("migrate");
        let account_id = seed_account(&control, "alpha").await;
        link_stripe_customer(&control, &account_id, "cus_1")
            .await
            .expect("link customer");

        // A created/active subscription on 'pro' widens the plan.
        let sub = SubscriptionState {
            stripe_customer_id: "cus_1".into(),
            stripe_subscription_id: "sub_1".into(),
            plan_id: "pro".into(),
            status: "active".into(),
            current_period_start: chrono::Utc::now(),
            current_period_end: chrono::Utc::now() + chrono::Duration::days(30),
            cancel_at_period_end: false,
            subdomain: Some("alpha".into()),
        };
        apply_subscription_event(&control, &account_id, &sub)
            .await
            .expect("apply subscription");
        assert_eq!(plan_id(&control, &account_id).await.as_deref(), Some("pro"));
        assert_eq!(billing_state(&control, &account_id).await, "active");

        // A failed payment enters past_due.
        apply_payment_failed(&control, &account_id)
            .await
            .expect("payment failed");
        assert_eq!(billing_state(&control, &account_id).await, "past_due");

        // A succeeded payment clears it.
        apply_payment_succeeded(&control, &account_id)
            .await
            .expect("payment succeeded");
        assert_eq!(billing_state(&control, &account_id).await, "active");

        // Subscription deleted: drop to free, data retained, state active.
        apply_subscription_deleted(&control, &account_id)
            .await
            .expect("subscription deleted");
        assert_eq!(
            plan_id(&control, &account_id).await.as_deref(),
            Some("free")
        );
        assert_eq!(billing_state(&control, &account_id).await, "active");

        // The audit trail recorded the transitions.
        let transitions: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM plan_transitions WHERE account_id = $1")
                .bind(&account_id)
                .fetch_one(control.pool())
                .await
                .expect("count transitions");
        assert!(
            transitions >= 3,
            "every transition is audited, got {transitions}"
        );
    })
    .await;
}

/// The production billing happy path, end-to-end through the real webhook
/// route: a brand-new Stripe customer whose `stripe_customers` row does NOT
/// yet exist (the redirect path never writes one — the webhook is the source
/// of truth). The `customer.subscription.created` event carries the account's
/// subdomain in its metadata (stamped at checkout), and the handler must use
/// it to ESTABLISH the linkage before applying — otherwise the plan never
/// widens and the subscription is silently dropped. This is the regression
/// test for the auto-link wiring; the other lifecycle tests pre-seed the link
/// directly and so never exercise this seam.
#[actix_web::test]
async fn webhook_auto_links_a_fresh_customer_and_widens_the_plan() {
    with_control_db("webhook_auto_link", |url| async move {
        const WEBHOOK_SECRET: &str = "whsec_e2e_secret";
        let control = ControlPlane::connect(&url).await.expect("connect");
        control.initialize().await.expect("migrate");

        // A real account, provisioned the normal way (plan_id starts 'free').
        let account_id = seed_account(&control, "alpha").await;

        // Billing wired with a present provider (so the webhook route is
        // enabled) and a real signing secret — but NO stripe_customers row.
        let billing = atomic_cloud::Billing::with_provider(
            control.clone(),
            Some(Arc::new(StubBillingProvider)),
            WEBHOOK_SECRET,
            HashMap::from([("pro".to_string(), "price_pro".to_string())]),
            format!("https://app.{BASE_DOMAIN}"),
            BASE_DOMAIN,
        );
        let harness =
            Harness::spawn_with_billing(control.clone(), DataPlaneRateLimits::default(), billing)
                .await;

        // A genuine first `customer.subscription.created` for a customer we've
        // never seen, carrying the subdomain Stripe echoes back from checkout.
        let now = chrono::Utc::now().timestamp();
        let payload = json!({
            "id": "evt_sub_created",
            "type": "customer.subscription.created",
            "data": { "object": {
                "id": "sub_new",
                "customer": "cus_brand_new",
                "status": "active",
                "cancel_at_period_end": false,
                "current_period_start": now,
                "current_period_end": now + 2_592_000,
                "metadata": { "subdomain": "alpha" },
                "items": { "data": [ { "price": {
                    "id": "price_pro",
                    "metadata": { "plan_id": "pro" }
                } } ] }
            }}
        });
        let body = serde_json::to_vec(&payload).expect("serialize");
        let signature = sign_webhook(WEBHOOK_SECRET, &body, now);

        let resp = harness
            .client
            .post(format!("{}/billing/webhook", harness.base_url))
            // The webhook lives on the app host, not a tenant subdomain.
            .header(HOST, format!("app.{BASE_DOMAIN}"))
            .header("stripe-signature", signature)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .expect("post webhook");
        assert_eq!(resp.status(), StatusCode::OK, "webhook accepted");

        // The plan widened end-to-end…
        assert_eq!(
            plan_id(&control, &account_id).await.as_deref(),
            Some("pro"),
            "checkout subscription widened the plan via auto-link"
        );
        // …and the linkage now exists, so subsequent events (portal, dunning)
        // resolve by customer id without the subdomain.
        let linked: Option<String> = sqlx::query_scalar(
            "SELECT account_id FROM stripe_customers WHERE stripe_customer_id = 'cus_brand_new'",
        )
        .fetch_optional(control.pool())
        .await
        .expect("query linkage");
        assert_eq!(
            linked.as_deref(),
            Some(account_id.as_str()),
            "the customer is now linked to its account"
        );
        harness.stop().await;
    })
    .await;
}

#[tokio::test]
async fn webhook_event_id_is_claimed_exactly_once() {
    with_control_db("webhook_claim_once", |url| async move {
        let control = ControlPlane::connect(&url).await.expect("connect");
        control.initialize().await.expect("migrate");
        let account_id = seed_account(&control, "alpha").await;

        // First delivery wins the claim; the redelivery (same id) does not.
        assert!(
            claim_webhook_event(&control, "evt_1", "customer.subscription.updated")
                .await
                .expect("first claim"),
            "first delivery claims the event"
        );
        assert!(
            !claim_webhook_event(&control, "evt_1", "customer.subscription.updated")
                .await
                .expect("second claim"),
            "redelivery of the same id is deduped"
        );

        // A distinct id is its own claim.
        assert!(
            claim_webhook_event(&control, "evt_2", "invoice.payment_failed")
                .await
                .expect("distinct claim"),
            "a different event id claims independently"
        );

        // Releasing a claim (the apply-failed compensation) lets a retry
        // re-claim it — the side effects never landed, so the retry must run.
        release_webhook_event(&control, "evt_1")
            .await
            .expect("release");
        assert!(
            claim_webhook_event(&control, "evt_1", "customer.subscription.updated")
                .await
                .expect("re-claim after release"),
            "a released event can be claimed again"
        );

        let _ = account_id;
    })
    .await;
}

#[tokio::test]
async fn replayed_subscription_event_does_not_duplicate_the_audit_row() {
    // The 'checkout' arm of apply_subscription_event records a plan_transitions
    // row unconditionally, so a *verbatim* replay would append a duplicate
    // audit row each time. The webhook handler's claim collapses redelivery to
    // a no-op; this proves the underlying double-apply is what the claim
    // guards against by asserting the count grows by exactly one per UNIQUE
    // event (the handler skips apply entirely on a claimed id).
    with_control_db("webhook_replay_audit", |url| async move {
        let control = ControlPlane::connect(&url).await.expect("connect");
        control.initialize().await.expect("migrate");
        let account_id = seed_account(&control, "alpha").await;
        link_stripe_customer(&control, &account_id, "cus_1")
            .await
            .expect("link customer");

        let sub = SubscriptionState {
            stripe_customer_id: "cus_1".into(),
            stripe_subscription_id: "sub_1".into(),
            plan_id: "pro".into(),
            status: "active".into(),
            current_period_start: chrono::Utc::now(),
            current_period_end: chrono::Utc::now() + chrono::Duration::days(30),
            cancel_at_period_end: false,
            subdomain: Some("alpha".into()),
        };

        // Simulate the handler's claim-then-apply for the SAME event id twice:
        // the first delivery applies, the redelivery is deduped before apply.
        for _ in 0..2 {
            if claim_webhook_event(&control, "evt_sub_1", "customer.subscription.updated")
                .await
                .expect("claim")
            {
                apply_subscription_event(&control, &account_id, &sub)
                    .await
                    .expect("apply");
            }
        }

        let transitions: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM plan_transitions WHERE account_id = $1")
                .bind(&account_id)
                .fetch_one(control.pool())
                .await
                .expect("count transitions");
        assert_eq!(
            transitions, 1,
            "the redelivered event is deduped — exactly one audit row"
        );
    })
    .await;
}

#[tokio::test]
async fn webhook_claim_rolls_back_with_a_failed_apply() {
    // The claim and the apply share ONE transaction, so a crash/error after
    // the claim but before commit must roll the claim back — otherwise a
    // committed-but-uneffected claim would dedupe Stripe's redelivery into a
    // permanent no-op and silently drop the event's billing effects (the
    // adversarial finding). This drives the exact tx shape the webhook handler
    // uses: claim_on_conn → apply_on_conn, then deliberately abort.
    with_control_db("webhook_claim_rollback", |url| async move {
        let control = ControlPlane::connect(&url).await.expect("connect");
        control.initialize().await.expect("migrate");
        let account_id = seed_account(&control, "alpha").await;
        link_stripe_customer(&control, &account_id, "cus_1")
            .await
            .expect("link customer");

        let sub = SubscriptionState {
            stripe_customer_id: "cus_1".into(),
            stripe_subscription_id: "sub_1".into(),
            plan_id: "pro".into(),
            status: "active".into(),
            current_period_start: chrono::Utc::now(),
            current_period_end: chrono::Utc::now() + chrono::Duration::days(30),
            cancel_at_period_end: false,
            subdomain: Some("alpha".into()),
        };

        // Delivery 1: claim + apply in a transaction, then ROLL BACK (simulating
        // a crash between the claim commit and the apply's side effects landing
        // in the old claim-before-apply design).
        {
            let mut tx = control.pool().begin().await.expect("begin");
            assert!(
                claim_webhook_event_on_conn(&mut tx, "evt_x", "customer.subscription.updated")
                    .await
                    .expect("claim in tx"),
                "first delivery claims the event"
            );
            apply_subscription_event_on_conn(&mut tx, &account_id, &sub)
                .await
                .expect("apply in tx");
            tx.rollback().await.expect("rollback");
        }

        // The claim was rolled back with the apply: the plan did NOT change and
        // no audit row exists — nothing committed.
        assert_eq!(
            plan_id(&control, &account_id).await.as_deref(),
            Some("free"),
            "the rolled-back apply left the plan unchanged"
        );
        let transitions: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM plan_transitions WHERE account_id = $1")
                .bind(&account_id)
                .fetch_one(control.pool())
                .await
                .expect("count transitions");
        assert_eq!(transitions, 0, "no audit row survived the rollback");

        // Stripe's redelivery of the SAME event id is therefore re-processed,
        // not deduped into a no-op: the claim is available again and this time
        // we commit.
        {
            let mut tx = control.pool().begin().await.expect("begin");
            assert!(
                claim_webhook_event_on_conn(&mut tx, "evt_x", "customer.subscription.updated")
                    .await
                    .expect("re-claim in tx"),
                "the rolled-back claim is available to the redelivery"
            );
            apply_subscription_event_on_conn(&mut tx, &account_id, &sub)
                .await
                .expect("apply in tx");
            tx.commit().await.expect("commit");
        }
        assert_eq!(
            plan_id(&control, &account_id).await.as_deref(),
            Some("pro"),
            "the redelivery applied the subscription"
        );

        // And a genuine redelivery AFTER a successful commit is deduped — the
        // claim is now committed, so a third delivery wins no claim and skips
        // apply (the audit row count stays at exactly one).
        {
            let mut tx = control.pool().begin().await.expect("begin");
            assert!(
                !claim_webhook_event_on_conn(&mut tx, "evt_x", "customer.subscription.updated")
                    .await
                    .expect("dup claim in tx"),
                "a committed claim dedupes the genuine redelivery"
            );
            tx.rollback().await.expect("rollback no-op");
        }
        let transitions: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM plan_transitions WHERE account_id = $1")
                .bind(&account_id)
                .fetch_one(control.pool())
                .await
                .expect("count transitions");
        assert_eq!(
            transitions, 1,
            "exactly one audit row: the committed delivery, deduped thereafter"
        );
    })
    .await;
}

#[tokio::test]
async fn dunning_advances_read_only_then_suspended_on_a_manufactured_clock() {
    with_control_db("dunning_time_machine", |url| async move {
        let control = ControlPlane::connect(&url).await.expect("connect");
        control.initialize().await.expect("migrate");
        let account_id = seed_account(&control, "alpha").await;

        // Enter past_due, then backdate past_due_since by 4 days: a sweep at
        // "now" must advance past_due → read_only (3-day threshold) but NOT
        // yet suspended (14-day threshold).
        apply_payment_failed(&control, &account_id)
            .await
            .expect("payment failed");
        backdate_past_due(&control, &account_id, 4).await;
        let advance = advance_dunning(&control, chrono::Utc::now())
            .await
            .expect("advance");
        assert_eq!(advance.moved_to_read_only, 1);
        assert_eq!(advance.moved_to_suspended, 0);
        assert_eq!(billing_state(&control, &account_id).await, "read_only");

        // Backdate to 15 days: the next sweep suspends (data retained).
        backdate_past_due(&control, &account_id, 15).await;
        let advance = advance_dunning(&control, chrono::Utc::now())
            .await
            .expect("advance");
        assert_eq!(advance.moved_to_suspended, 1);
        assert_eq!(billing_state(&control, &account_id).await, "suspended");

        // The account row — and its data — still exist (never auto-deleted).
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id = $1)")
                .bind(&account_id)
                .fetch_one(control.pool())
                .await
                .expect("exists");
        assert!(exists, "suspended account is retained, never deleted");
    })
    .await;
}

/// `start_trial` puts a pristine account on the paid tier in the `trialing`
/// state with a future `trial_ends_at`, is first-time-only and idempotent, and
/// never resets an account that already moved off free (a converted trial).
#[tokio::test]
async fn start_trial_is_first_time_only_and_idempotent() {
    with_control_db("trial_start", |url| async move {
        let control = ControlPlane::connect(&url).await.expect("connect");
        control.initialize().await.expect("migrate");
        let account_id = seed_account(&control, "alpha").await;

        // First call starts the trial: trialing, on 'pro', deadline in ~14d.
        let started = start_trial(&control, &account_id, "pro", chrono::Duration::days(14))
            .await
            .expect("start trial");
        assert!(started, "a pristine account starts a trial");
        assert_eq!(billing_state(&control, &account_id).await, "trialing");
        assert_eq!(plan_id(&control, &account_id).await.as_deref(), Some("pro"));
        let ends = trial_ends_at(&control, &account_id)
            .await
            .expect("deadline set");
        let in_days = (ends - chrono::Utc::now()).num_days();
        assert!(
            (12..=14).contains(&in_days),
            "deadline ~14 days out, got {in_days}"
        );

        // A second call is a no-op — the trial is already running.
        let again = start_trial(&control, &account_id, "pro", chrono::Duration::days(14))
            .await
            .expect("start trial again");
        assert!(!again, "an already-trialing account is not re-trialed");

        // An account that has already moved off free (e.g. a paid checkout)
        // is never silently reset into a trial.
        set_billing_state(&control, &account_id, "active").await;
        sqlx::query("UPDATE accounts SET plan_id = 'pro', trial_ends_at = NULL WHERE id = $1")
            .bind(&account_id)
            .execute(control.pool())
            .await
            .expect("simulate paid");
        let on_paid = start_trial(&control, &account_id, "pro", chrono::Duration::days(14))
            .await
            .expect("start trial on paid");
        assert!(!on_paid, "a paid account is never reset into a trial");
        assert_eq!(billing_state(&control, &account_id).await, "active");
        assert!(
            trial_ends_at(&control, &account_id).await.is_none(),
            "no trial deadline stamped on a paid account"
        );
    })
    .await;
}

/// An expired trial auto-downgrades to the free plan: `active` when the
/// account is under the free limits, `read_only` when over (over-limit data
/// retained, never deleted) — the control-plane half driven directly, the
/// over-limit decision injected exactly as the sweep would feed it.
#[tokio::test]
async fn expired_trial_downgrades_to_free_active_or_read_only() {
    with_control_db("trial_downgrade", |url| async move {
        let control = ControlPlane::connect(&url).await.expect("connect");
        control.initialize().await.expect("migrate");

        // Under-limit account: trial expires → free + active.
        let under = seed_account(&control, "under").await;
        start_trial(&control, &under, "pro", chrono::Duration::days(14))
            .await
            .expect("start trial");
        expire_trial(&control, &under, 1).await;
        let downgraded = finish_expired_trial(&control, &under, false)
            .await
            .expect("finish under-limit trial");
        assert!(downgraded);
        assert_eq!(plan_id(&control, &under).await.as_deref(), Some("free"));
        assert_eq!(billing_state(&control, &under).await, "active");
        assert!(
            trial_ends_at(&control, &under).await.is_none(),
            "trial deadline cleared on downgrade"
        );

        // Over-limit account: trial expires → free + read_only.
        let over = seed_account(&control, "over").await;
        start_trial(&control, &over, "pro", chrono::Duration::days(14))
            .await
            .expect("start trial");
        expire_trial(&control, &over, 1).await;
        let downgraded = finish_expired_trial(&control, &over, true)
            .await
            .expect("finish over-limit trial");
        assert!(downgraded);
        assert_eq!(plan_id(&control, &over).await.as_deref(), Some("free"));
        assert_eq!(billing_state(&control, &over).await, "read_only");

        // Finishing again is a no-op (state is no longer trialing).
        let again = finish_expired_trial(&control, &over, true)
            .await
            .expect("finish again");
        assert!(!again, "a finished trial is not re-downgraded");
    })
    .await;
}

/// The sweep finds only *expired* trials and feeds each through the over-limit
/// predicate. A still-running trial is left alone; an expired one downgrades.
/// NEVER-DELETE invariant: the downgraded account's row is retained.
#[tokio::test]
async fn trial_sweep_downgrades_only_expired_and_never_deletes() {
    with_control_db("trial_sweep", |url| async move {
        let control = ControlPlane::connect(&url).await.expect("connect");
        control.initialize().await.expect("migrate");

        let running = seed_account(&control, "running").await;
        let expired = seed_account(&control, "expired").await;
        for id in [&running, &expired] {
            start_trial(&control, id, "pro", chrono::Duration::days(14))
                .await
                .expect("start trial");
        }
        // Only `expired` is past its deadline.
        expire_trial(&control, &expired, 1).await;

        // `expired_trials` returns exactly the past-deadline account.
        let due = expired_trials(&control, chrono::Utc::now())
            .await
            .expect("expired trials");
        assert_eq!(due, vec![expired.clone()]);

        // Drive the sweep with an over-limit predicate (so the downgrade lands
        // read_only) and assert it only touched the expired account.
        let advance = advance_expired_trials(&control, chrono::Utc::now(), |_id| async {
            Ok::<bool, atomic_cloud::CloudError>(true)
        })
        .await
        .expect("advance trials");
        assert_eq!(advance.downgraded_to_read_only, 1);
        assert_eq!(advance.downgraded_to_active, 0);

        // The expired account downgraded; the running one is untouched.
        assert_eq!(plan_id(&control, &expired).await.as_deref(), Some("free"));
        assert_eq!(billing_state(&control, &expired).await, "read_only");
        assert_eq!(plan_id(&control, &running).await.as_deref(), Some("pro"));
        assert_eq!(billing_state(&control, &running).await, "trialing");

        // NEVER-DELETE: both rows still exist after the sweep.
        for id in [&running, &expired] {
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id = $1)")
                    .bind(id)
                    .fetch_one(control.pool())
                    .await
                    .expect("exists");
            assert!(exists, "downgraded/trialing accounts are retained");
        }
    })
    .await;
}

/// A trialing account serves reads AND writes (a trial is full access); an
/// expired trial swept to `read_only` (over-limit) then blocks writes while
/// reads still pass — end-to-end through the composed server, with the
/// over-limit decision read from the real tenant database against a shrunk
/// free plan.
#[actix_web::test]
async fn trialing_account_has_full_access_then_sweep_downgrades_to_read_only() {
    with_control_db("trial_e2e", |url| async move {
        // Shrink the free plan so a single atom puts the account over the
        // free limit once it downgrades (the trial grants 'pro', unlimited).
        set_free_limits(&url, Some(0), None).await;
        let harness = Harness::spawn(&url, DataPlaneRateLimits::default()).await;
        let tenant = harness.provision("alpha").await;

        // Put the account on a trial (signup-completion does this in
        // production; here we drive it directly to keep the test on the
        // control-plane seam without exercising the magic-link flow).
        start_trial(
            &harness.control,
            &tenant.account_id,
            "pro",
            chrono::Duration::days(14),
        )
        .await
        .expect("start trial");
        assert_eq!(
            billing_state(&harness.control, &tenant.account_id).await,
            "trialing"
        );

        // Trial = full access: a write succeeds (the 'pro' plan is unlimited,
        // so the quota guard never blocks it).
        let write = harness.create_atom(&tenant, "during trial").await;
        assert_eq!(
            write.status(),
            StatusCode::CREATED,
            "a trial serves writes (full access)"
        );

        // Expire the trial and run the sweep with the real tenant-aware
        // over-limit check: the account now holds 1 atom against a free
        // atom_limit of 0, so it downgrades to read_only.
        expire_trial(&harness.control, &tenant.account_id, 1).await;
        let plan_registry = PlanRegistry::load(harness.control.clone())
            .await
            .expect("load plans");
        let free_plan = plan_registry.get("free").expect("free plan");
        let cache = Arc::new(AccountCache::new(
            harness.control.clone(),
            harness.cluster.clone(),
            support::test_vault(),
            AccountCacheConfig::default(),
        ));
        let advance = advance_expired_trials(&harness.control, chrono::Utc::now(), |account_id| {
            let cache = Arc::clone(&cache);
            let free_plan = free_plan.clone();
            async move {
                let handle = cache.get_or_load(&account_id).await?;
                atomic_cloud::account_over_plan_limits(&free_plan, &handle.manager)
                    .await
                    .map_err(|e| atomic_cloud::CloudError::Invariant(e.to_string()))
            }
        })
        .await
        .expect("advance trials");
        assert_eq!(advance.downgraded_to_read_only, 1);
        assert_eq!(
            billing_state(&harness.control, &tenant.account_id).await,
            "read_only"
        );

        // read_only: reads pass, writes 402 (data retained, never deleted).
        let read = harness
            .api(Method::GET, &tenant.subdomain, "/api/atoms")
            .bearer_auth(&tenant.token)
            .send()
            .await
            .expect("read");
        assert_eq!(
            read.status(),
            StatusCode::OK,
            "downgraded account still reads"
        );
        let blocked = harness.create_atom(&tenant, "after downgrade").await;
        assert_eq!(
            blocked.status(),
            StatusCode::PAYMENT_REQUIRED,
            "read_only blocks writes after trial downgrade"
        );
        let body: Value = blocked.json().await.expect("body");
        assert_eq!(body["error"], "account_read_only");

        // The atom created during the trial is retained (never deleted).
        let list = harness
            .api(Method::GET, &tenant.subdomain, "/api/atoms")
            .bearer_auth(&tenant.token)
            .send()
            .await
            .expect("list");
        let atoms: Value = list.json().await.expect("atoms");
        assert!(
            atoms.as_array().map(|a| !a.is_empty()).unwrap_or(false)
                || atoms["atoms"]
                    .as_array()
                    .map(|a| !a.is_empty())
                    .unwrap_or(false),
            "the trial-era atom survives the downgrade"
        );

        harness.stop().await;
    })
    .await;
}

/// Recovery: a `read_only` account whose payment succeeds returns to `active`
/// and can write again — the dunning recovery path, end-to-end.
#[actix_web::test]
async fn payment_succeeded_lifts_read_only_and_restores_writes() {
    with_control_db("billing_recovery", |url| async move {
        let harness = Harness::spawn(&url, DataPlaneRateLimits::default()).await;
        let tenant = harness.provision("alpha").await;

        // Drive into read_only and prove a write is blocked.
        set_billing_state(&harness.control, &tenant.account_id, "read_only").await;
        let blocked = harness.create_atom(&tenant, "blocked").await;
        assert_eq!(blocked.status(), StatusCode::PAYMENT_REQUIRED);

        // Payment succeeds → billing_state clears to active.
        apply_payment_succeeded(&harness.control, &tenant.account_id)
            .await
            .expect("payment succeeded");
        assert_eq!(
            billing_state(&harness.control, &tenant.account_id).await,
            "active"
        );

        // Writes work again.
        let ok = harness.create_atom(&tenant, "recovered").await;
        assert_eq!(
            ok.status(),
            StatusCode::CREATED,
            "a recovered account can write again"
        );

        harness.stop().await;
    })
    .await;
}

/// Gate ordering: a `suspended` account that is ALSO credits-paused gets the
/// `account_suspended` response, not `out_of_ai_credits`. Suspended is the
/// more terminal state and is gated first in CloudAuth, before the request
/// ever reaches the downstream out-of-credits guard.
#[actix_web::test]
async fn suspended_wins_over_credits_pause() {
    with_control_db("billing_gate_order", |url| async move {
        let harness = Harness::spawn(&url, DataPlaneRateLimits::default()).await;
        let tenant = harness.provision("alpha").await;

        set_billing_state(&harness.control, &tenant.account_id, "suspended").await;
        set_credits_paused(&harness.control, &tenant.account_id).await;

        let resp = harness
            .api(Method::GET, &tenant.subdomain, "/api/atoms")
            .bearer_auth(&tenant.token)
            .send()
            .await
            .expect("request");
        assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
        let body: Value = resp.json().await.expect("body");
        assert_eq!(
            body["error"], "account_suspended",
            "suspended is gated before the credits pause"
        );

        harness.stop().await;
    })
    .await;
}

#[actix_web::test]
async fn suspended_account_is_blocked_at_auth_read_only_blocks_writes() {
    with_control_db("billing_serving_gate", |url| async move {
        let harness = Harness::spawn(&url, DataPlaneRateLimits::default()).await;
        let tenant = harness.provision("alpha").await;

        // read_only: reads pass, writes 402.
        set_billing_state(&harness.control, &tenant.account_id, "read_only").await;
        let read = harness
            .api(Method::GET, &tenant.subdomain, "/api/atoms")
            .bearer_auth(&tenant.token)
            .send()
            .await
            .expect("read");
        assert_eq!(
            read.status(),
            StatusCode::OK,
            "read_only still serves reads"
        );
        let write = harness.create_atom(&tenant, "blocked").await;
        assert_eq!(
            write.status(),
            StatusCode::PAYMENT_REQUIRED,
            "read_only blocks writes"
        );
        let body: Value = write.json().await.expect("body");
        assert_eq!(body["error"], "account_read_only");

        // suspended: even reads are blocked at auth (data retained).
        set_billing_state(&harness.control, &tenant.account_id, "suspended").await;
        let read = harness
            .api(Method::GET, &tenant.subdomain, "/api/atoms")
            .bearer_auth(&tenant.token)
            .send()
            .await
            .expect("read");
        assert_eq!(
            read.status(),
            StatusCode::PAYMENT_REQUIRED,
            "suspended blocks serving"
        );
        let body: Value = read.json().await.expect("body");
        assert_eq!(body["error"], "account_suspended");

        harness.stop().await;
    })
    .await;
}

/// The checkout route, driven against a scripted provider, returns a 302 to
/// the session URL — and is account-scope-gated exactly like
/// `DELETE /api/account`: a database-scoped token (KB-pinned) gets 403
/// `account_scope_required`, never a redirect or a Stripe call.
#[actix_web::test]
async fn checkout_route_redirects_and_is_account_scope_gated() {
    with_control_db("billing_checkout_route", |url| async move {
        let control = ControlPlane::connect(&url).await.expect("connect");
        control.initialize().await.expect("migrate");
        let billing = atomic_cloud::Billing::with_provider(
            control.clone(),
            Some(Arc::new(RecordingBilling::new(
                "https://checkout.stripe.test/cs_route",
                "https://portal.stripe.test/ps_route",
            ))),
            "whsec_route",
            HashMap::from([("pro".to_string(), "price_pro".to_string())]),
            format!("https://app.{BASE_DOMAIN}"),
            BASE_DOMAIN,
        );
        let harness =
            Harness::spawn_with_billing(control.clone(), DataPlaneRateLimits::default(), billing)
                .await;
        let tenant = harness.provision("alpha").await;

        // The recording provider's session URL is on a non-resolvable host, so
        // a client that FOLLOWS the 302 would fail at connect. Use a no-redirect
        // client and assert on the 302 + Location header directly.
        let no_redirect = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("no-redirect client");

        // Account-scope token → 302 into Stripe Checkout, no plan state written.
        let resp = no_redirect
            .get(format!(
                "{}/api/billing/checkout?plan=pro",
                harness.base_url
            ))
            .header(HOST, format!("{}.{BASE_DOMAIN}", tenant.subdomain))
            .bearer_auth(&tenant.token)
            .send()
            .await
            .expect("checkout");
        assert_eq!(resp.status(), StatusCode::FOUND, "checkout 302s");
        assert_eq!(
            resp.headers().get("location").and_then(|v| v.to_str().ok()),
            Some("https://checkout.stripe.test/cs_route"),
            "redirects to the Stripe Checkout session URL"
        );

        // Unknown plan → 400 unknown_plan (still account-scope, reached the route).
        let resp = harness
            .api(
                Method::GET,
                &tenant.subdomain,
                "/api/billing/checkout?plan=nope",
            )
            .bearer_auth(&tenant.token)
            .send()
            .await
            .expect("checkout bad plan");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body: Value = resp.json().await.expect("body");
        assert_eq!(body["error"], "unknown_plan");

        // A database-scoped token is KB-pinned → 403, never a redirect.
        let db_token = issue_token(
            &harness.control,
            &tenant.account_id,
            TokenScope::Database,
            Some("default"),
            "kb-pinned",
        )
        .await
        .expect("issue db token");
        let resp = harness
            .api(
                Method::GET,
                &tenant.subdomain,
                "/api/billing/checkout?plan=pro",
            )
            .bearer_auth(&db_token)
            .send()
            .await
            .expect("db-scoped checkout");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "db-scoped 403s");
        let body: Value = resp.json().await.expect("body");
        assert_eq!(body["error"], "account_scope_required");

        harness.stop().await;
    })
    .await;
}

/// The portal route 409s when the account has no Stripe customer yet, 302s
/// into the portal once linked, and is account-scope-gated (db-scoped → 403).
#[actix_web::test]
async fn portal_route_conflicts_redirects_and_is_account_scope_gated() {
    with_control_db("billing_portal_route", |url| async move {
        let control = ControlPlane::connect(&url).await.expect("connect");
        control.initialize().await.expect("migrate");
        let billing = atomic_cloud::Billing::with_provider(
            control.clone(),
            Some(Arc::new(RecordingBilling::new(
                "https://checkout.stripe.test/cs_route",
                "https://portal.stripe.test/ps_route",
            ))),
            "whsec_route",
            HashMap::from([("pro".to_string(), "price_pro".to_string())]),
            format!("https://app.{BASE_DOMAIN}"),
            BASE_DOMAIN,
        );
        let harness =
            Harness::spawn_with_billing(control.clone(), DataPlaneRateLimits::default(), billing)
                .await;
        let tenant = harness.provision("alpha").await;

        // No Stripe customer yet → 409 no_billing_customer (must check out first).
        let resp = harness
            .api(Method::GET, &tenant.subdomain, "/api/billing/portal")
            .bearer_auth(&tenant.token)
            .send()
            .await
            .expect("portal pre-link");
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body: Value = resp.json().await.expect("body");
        assert_eq!(body["error"], "no_billing_customer");

        // Link a customer, then the portal 302s into Stripe. Use a no-redirect
        // client (the session URL host doesn't resolve) and assert the 302.
        link_stripe_customer(&harness.control, &tenant.account_id, "cus_portal")
            .await
            .expect("link customer");
        let no_redirect = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("no-redirect client");
        let resp = no_redirect
            .get(format!("{}/api/billing/portal", harness.base_url))
            .header(HOST, format!("{}.{BASE_DOMAIN}", tenant.subdomain))
            .bearer_auth(&tenant.token)
            .send()
            .await
            .expect("portal post-link");
        assert_eq!(resp.status(), StatusCode::FOUND, "portal 302s");
        assert_eq!(
            resp.headers().get("location").and_then(|v| v.to_str().ok()),
            Some("https://portal.stripe.test/ps_route"),
            "redirects to the Stripe Customer Portal session URL"
        );

        // A database-scoped token → 403, never a redirect.
        let db_token = issue_token(
            &harness.control,
            &tenant.account_id,
            TokenScope::Database,
            Some("default"),
            "kb-pinned",
        )
        .await
        .expect("issue db token");
        let resp = harness
            .api(Method::GET, &tenant.subdomain, "/api/billing/portal")
            .bearer_auth(&db_token)
            .send()
            .await
            .expect("db-scoped portal");
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body: Value = resp.json().await.expect("body");
        assert_eq!(body["error"], "account_scope_required");

        harness.stop().await;
    })
    .await;
}

/// The webhook rejects a bad/missing signature with 400 BEFORE any parsing or
/// lookup (only a forger ever sees it), and applies no state.
#[actix_web::test]
async fn webhook_rejects_a_bad_signature_with_400() {
    with_control_db("billing_webhook_badsig", |url| async move {
        const SECRET: &str = "whsec_badsig";
        let control = ControlPlane::connect(&url).await.expect("connect");
        control.initialize().await.expect("migrate");
        let account_id = seed_account(&control, "alpha").await;
        link_stripe_customer(&control, &account_id, "cus_sig")
            .await
            .expect("link customer");

        let billing = atomic_cloud::Billing::with_provider(
            control.clone(),
            Some(Arc::new(RecordingBilling::new("u", "u"))),
            SECRET,
            HashMap::new(),
            format!("https://app.{BASE_DOMAIN}"),
            BASE_DOMAIN,
        );
        let harness =
            Harness::spawn_with_billing(control.clone(), DataPlaneRateLimits::default(), billing)
                .await;

        // A payload that, if it were applied, would move this account to
        // past_due — proving the 400 short-circuited before any effect.
        let payload = serde_json::to_vec(&json!({
            "id": "evt_badsig",
            "type": "invoice.payment_failed",
            "data": { "object": { "customer": "cus_sig" } }
        }))
        .expect("serialize");

        let now = chrono::Utc::now().timestamp();
        let cases: Vec<(&str, Option<String>)> = vec![
            // No signature header at all.
            ("missing", None),
            // A header that doesn't parse as Stripe's `t=,v1=` scheme.
            ("garbage", Some("not-a-signature".to_string())),
            // A correctly-shaped, correctly-timed signature — under the WRONG
            // secret, so the HMAC can't match.
            (
                "wrong-secret",
                Some(sign_webhook("whsec_other", &payload, now)),
            ),
            // A correctly-signed header but a stale timestamp (replay defense).
            ("stale", Some(sign_webhook(SECRET, &payload, now - 10_000))),
        ];
        for (label, header) in cases {
            let mut req = harness
                .client
                .post(format!("{}/billing/webhook", harness.base_url))
                .header(HOST, format!("app.{BASE_DOMAIN}"))
                .header("content-type", "application/json")
                .body(payload.clone());
            if let Some(h) = header {
                req = req.header("stripe-signature", h);
            }
            let resp = req.send().await.expect("post webhook");
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "{label} signature rejected with 400"
            );
            let body: Value = resp.json().await.expect("body");
            assert_eq!(body["error"], "invalid_signature", "{label}");
        }

        // No state changed: the account is still active, no dedup row written.
        assert_eq!(billing_state(&control, &account_id).await, "active");
        let claimed: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM processed_webhook_events WHERE event_id = 'evt_badsig'",
        )
        .fetch_one(control.pool())
        .await
        .expect("count claims");
        assert_eq!(claimed, 0, "a rejected signature claims no event id");

        harness.stop().await;
    })
    .await;
}

/// A genuinely-signed event for a Stripe customer mapped to NO account is
/// acked 200 (so Stripe stops retrying a permanently-orphaned event) with no
/// state change — Stripe best practice for an event we cannot correlate.
#[actix_web::test]
async fn webhook_orphaned_customer_is_acked_200_with_no_effect() {
    with_control_db("billing_webhook_orphan", |url| async move {
        const SECRET: &str = "whsec_orphan";
        let control = ControlPlane::connect(&url).await.expect("connect");
        control.initialize().await.expect("migrate");

        let billing = atomic_cloud::Billing::with_provider(
            control.clone(),
            Some(Arc::new(RecordingBilling::new("u", "u"))),
            SECRET,
            HashMap::new(),
            format!("https://app.{BASE_DOMAIN}"),
            BASE_DOMAIN,
        );
        let harness =
            Harness::spawn_with_billing(control.clone(), DataPlaneRateLimits::default(), billing)
                .await;

        // payment_failed for a customer that maps to no account at all.
        let now = chrono::Utc::now().timestamp();
        let payload = serde_json::to_vec(&json!({
            "id": "evt_orphan",
            "type": "invoice.payment_failed",
            "data": { "object": { "customer": "cus_nobody" } }
        }))
        .expect("serialize");
        let signature = sign_webhook(SECRET, &payload, now);

        let resp = harness
            .client
            .post(format!("{}/billing/webhook", harness.base_url))
            .header(HOST, format!("app.{BASE_DOMAIN}"))
            .header("stripe-signature", signature)
            .header("content-type", "application/json")
            .body(payload)
            .send()
            .await
            .expect("post webhook");
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "orphaned event is acked so Stripe stops retrying"
        );

        // The event was claimed (so a redelivery is a fast no-op) but no
        // account state was touched — there was no account to touch.
        let claimed: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM processed_webhook_events WHERE event_id = 'evt_orphan'",
        )
        .fetch_one(control.pool())
        .await
        .expect("count claims");
        assert_eq!(claimed, 1, "the orphaned event id is claimed");

        harness.stop().await;
    })
    .await;
}

// --- helpers -----------------------------------------------------------------

/// A `BillingProvider` test double: returns scripted session URLs and records
/// the calls it received. Drives the checkout/portal route tests without a
/// Stripe account (the trait is the seam; the wiremock test in
/// `tests/stripe_client.rs` pins the real client's request shape).
struct RecordingBilling {
    checkout_url: String,
    portal_url: String,
    calls: std::sync::Mutex<Vec<String>>,
}

impl RecordingBilling {
    fn new(checkout_url: impl Into<String>, portal_url: impl Into<String>) -> Self {
        Self {
            checkout_url: checkout_url.into(),
            portal_url: portal_url.into(),
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl atomic_cloud::BillingProvider for RecordingBilling {
    async fn create_checkout_session(
        &self,
        price_id: &str,
        _customer_email: &str,
        subdomain: &str,
        _success_url: &str,
        _cancel_url: &str,
    ) -> Result<atomic_cloud::StripeSession, atomic_cloud::CloudError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("checkout:{price_id}:{subdomain}"));
        Ok(atomic_cloud::StripeSession {
            url: self.checkout_url.clone(),
        })
    }

    async fn create_portal_session(
        &self,
        stripe_customer_id: &str,
        _return_url: &str,
    ) -> Result<atomic_cloud::StripeSession, atomic_cloud::CloudError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("portal:{stripe_customer_id}"));
        Ok(atomic_cloud::StripeSession {
            url: self.portal_url.clone(),
        })
    }
}

/// A `BillingProvider` whose presence alone enables the webhook route. The
/// webhook handler never calls these (it only verifies + projects + applies);
/// the auto-link webhook test doesn't start a checkout/portal session, so the
/// methods just need to exist.
struct StubBillingProvider;

#[async_trait::async_trait]
impl atomic_cloud::BillingProvider for StubBillingProvider {
    async fn create_checkout_session(
        &self,
        _price_id: &str,
        _customer_email: &str,
        _subdomain: &str,
        _success_url: &str,
        _cancel_url: &str,
    ) -> Result<atomic_cloud::StripeSession, atomic_cloud::CloudError> {
        unreachable!("the webhook test does not start a checkout session")
    }

    async fn create_portal_session(
        &self,
        _stripe_customer_id: &str,
        _return_url: &str,
    ) -> Result<atomic_cloud::StripeSession, atomic_cloud::CloudError> {
        unreachable!("the webhook test does not start a portal session")
    }
}

/// Build a valid `Stripe-Signature` header for `payload` at time `t` under
/// `secret` — the exact HMAC-SHA256-over-`"{t}.{body}"` scheme
/// `verify_webhook` checks, so the e2e test posts a genuinely signed body.
fn sign_webhook(secret: &str, payload: &[u8], t: i64) -> String {
    use hmac::{Hmac, Mac};
    let mut signed = Vec::new();
    signed.extend_from_slice(t.to_string().as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(payload);
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(&signed);
    let hex = data_encoding::HEXLOWER.encode(&mac.finalize().into_bytes());
    format!("t={t},v1={hex}")
}

/// Seed a bare active account directly (the dunning/lifecycle tests don't
/// need a tenant database — they exercise control-plane state only).
async fn seed_account(control: &ControlPlane, subdomain: &str) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO accounts (id, subdomain, email, status, plan, plan_id) \
         VALUES ($1, $2, $3, 'active', 'free', 'free')",
    )
    .bind(&id)
    .bind(subdomain)
    .bind(format!("{subdomain}@example.com"))
    .execute(control.pool())
    .await
    .expect("seed account");
    id
}

async fn plan_id(control: &ControlPlane, account_id: &str) -> Option<String> {
    let row: Option<String> = sqlx::query_scalar("SELECT plan_id FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(control.pool())
        .await
        .expect("plan_id");
    row
}

async fn billing_state(control: &ControlPlane, account_id: &str) -> String {
    sqlx::query_scalar("SELECT billing_state FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(control.pool())
        .await
        .expect("billing_state")
}

async fn set_billing_state(control: &ControlPlane, account_id: &str, state: &str) {
    sqlx::query("UPDATE accounts SET billing_state = $2 WHERE id = $1")
        .bind(account_id)
        .bind(state)
        .execute(control.pool())
        .await
        .expect("set billing_state");
}

/// Backdate `past_due_since` by `days` so a sweep at the real `now` crosses
/// the threshold — the no-real-waits idiom.
async fn backdate_past_due(control: &ControlPlane, account_id: &str, days: i64) {
    sqlx::query(
        "UPDATE accounts SET past_due_since = NOW() - make_interval(days => $2) WHERE id = $1",
    )
    .bind(account_id)
    .bind(days as i32)
    .execute(control.pool())
    .await
    .expect("backdate past_due_since");
}

/// Move a trialing account's `trial_ends_at` to `days` in the past so a sweep
/// at the real `now` sees it as expired — the no-real-waits idiom (the trial
/// analogue of `backdate_past_due`).
async fn expire_trial(control: &ControlPlane, account_id: &str, days_ago: i64) {
    sqlx::query(
        "UPDATE accounts SET trial_ends_at = NOW() - make_interval(days => $2) WHERE id = $1",
    )
    .bind(account_id)
    .bind(days_ago as i32)
    .execute(control.pool())
    .await
    .expect("expire trial_ends_at");
}

/// Read `trial_ends_at` (NULL once the trial ends or never started).
async fn trial_ends_at(
    control: &ControlPlane,
    account_id: &str,
) -> Option<chrono::DateTime<chrono::Utc>> {
    sqlx::query_scalar("SELECT trial_ends_at FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(control.pool())
        .await
        .expect("trial_ends_at")
}

/// Force a credits pause active for `account_id` (the `out_of_ai_credits`
/// surface), so the gate-ordering test can prove `suspended` wins over a
/// credits pause. Mirrors the column shape backpressure.rs writes.
async fn set_credits_paused(control: &ControlPlane, account_id: &str) {
    sqlx::query(
        "UPDATE accounts \
            SET provider_paused_until = NOW() + interval '1 hour', \
                provider_pause_kind = 'credits', provider_pause_streak = 1 \
          WHERE id = $1",
    )
    .bind(account_id)
    .execute(control.pool())
    .await
    .expect("set credits pause");
}
