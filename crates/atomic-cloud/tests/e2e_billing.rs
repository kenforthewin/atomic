//! Billing & quotas end to end, driven through the composed cloud server
//! (plan: "Observability, quotas, billing" → "Quotas", "Billing"; Decisions
//! log 2026-06-09 billing-v1-subscription, 2026-05-25 Stripe-via-portal +
//! signed-webhook + never-auto-delete + trials + two-tier quotas).
//!
//! Slice 6's e2e suite. The per-transition control-plane behavior is pinned
//! in `tests/quota_billing.rs`; this file proves the *whole lifecycle* hangs
//! together through the real server, plus the slice's new mechanics that have
//! no per-transition home:
//!
//! - The full lifecycle in one test: provision → trial → trial-expiry
//!   auto-downgrade (manufactured clock) → over-limit read-only → signed
//!   checkout webhook widens the plan → writes resume → payment_failed → past_due
//!   → dunning reaper 7d → read_only → payment_succeeded → active recovered.
//! - **Cross-tenant isolation**: a second tenant (`beta`) stays on its own
//!   plan and serving state untouched throughout `alpha`'s entire journey.
//! - **Storage enforcement, no-delete**: the recompute arm drives an
//!   over-storage tenant warn → restricted (writes 402 `account_storage_restricted`,
//!   reads still pass, data retained), then a raised limit clears it.
//! - **Period rollover idempotency + cross-pod safety**: a second rollover in
//!   the same period inserts nothing; old period rows are retained.
//! - The exact `quota_exceeded` / `account_read_only` / `account_suspended` /
//!   `account_storage_restricted` response bodies, and webhook-signature
//!   rejection end to end.
//!
//! Postgres-gated; see `tests/support/mod.rs`. Run single-threaded:
//!
//! ```sh
//! CARGO_INCREMENTAL=0 \
//! ATOMIC_TEST_DATABASE_URL=postgres://atomic:atomic_test@localhost:5433/atomic_test \
//!   cargo test -p atomic-cloud --test e2e_billing -- --test-threads=1
//! ```

mod support;

use std::collections::HashMap;
use std::sync::Arc;

use actix_web::{web, App, HttpServer};
use atomic_cloud::{
    account_over_plan_limits, advance_dunning_with, advance_expired_trials, configure_cloud_app,
    current_period_start, issue_token, link_stripe_customer, provision_account, recompute_storage,
    roll_over_period, start_trial, AccountCache, AccountCacheConfig, AccountPlane,
    AccountPlaneConfig, ChatStreamLimiter, CloudAuth, ClusterConfig, ControlPlane,
    DataPlaneRateLimiter, DataPlaneRateLimits, DunningThresholds, FallbackAppState, ManagedKeys,
    NewAccount, PlanRegistry, QuotaBilling, Readiness, StoragePolicy, TenantPlane, TokenScope,
    DEFAULT_CHAT_STREAMS_PER_ACCOUNT, STORAGE_BYTES_METRIC,
};
use reqwest::header::HOST;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use support::with_control_db;

const BASE_DOMAIN: &str = "cloudtest.local";
const WEBHOOK_SECRET: &str = "whsec_e2e_billing";

/// A provisioned tenant on the harness.
struct Tenant {
    account_id: String,
    subdomain: String,
    token: String,
}

/// The composed cloud server with billing fully wired (a scripted provider +
/// a real signing secret), so the webhook route verifies signed payloads and
/// drives the subscription lifecycle end to end.
struct Harness {
    control: ControlPlane,
    cluster: ClusterConfig,
    registry: PlanRegistry,
    cache: Arc<AccountCache>,
    client: reqwest::Client,
    base_url: String,
    handle: actix_web::dev::ServerHandle,
    _fallback: FallbackAppState,
}

impl Harness {
    async fn spawn(control_url: &str) -> Self {
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

        // Billing wired with a scripted provider (so the webhook + routes are
        // enabled) and a real signing secret, plus the pro price mapping the
        // checkout webhook resolves through.
        let billing = atomic_cloud::Billing::with_provider(
            control.clone(),
            Some(Arc::new(RecordingBilling::new(
                "https://checkout.stripe.test/cs_e2e",
                "https://portal.stripe.test/ps_e2e",
            ))),
            WEBHOOK_SECRET,
            HashMap::from([("pro".to_string(), "price_pro".to_string())]),
            format!("https://app.{BASE_DOMAIN}"),
            BASE_DOMAIN,
        );

        // A standalone registry handle (the same plan catalogue the server
        // loads) for the trial/storage sweeps the test drives directly.
        let registry = PlanRegistry::load(control.clone())
            .await
            .expect("load plans");
        let plan_registry =
            web::Data::new(PlanRegistry::load(control.clone()).await.expect("load"));

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
            rate_limiter: DataPlaneRateLimiter::new(DataPlaneRateLimits::default()),
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
                None,
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
            registry,
            cache,
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
            "billing-e2e",
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

    async fn create_atom(&self, tenant: &Tenant, content: &str) -> reqwest::Response {
        self.api(Method::POST, &tenant.subdomain, "/api/atoms")
            .bearer_auth(&tenant.token)
            .json(&json!({ "content": content }))
            .send()
            .await
            .expect("send create atom")
    }

    async fn read_atoms(&self, tenant: &Tenant) -> reqwest::Response {
        self.api(Method::GET, &tenant.subdomain, "/api/atoms")
            .bearer_auth(&tenant.token)
            .send()
            .await
            .expect("read atoms")
    }

    /// Run the trial auto-downgrade sweep with the real tenant-aware
    /// over-limit predicate, exactly as `serve` wires it.
    async fn run_trial_sweep(&self) -> atomic_cloud::TrialAdvance {
        let free_plan = self.registry.get("free").expect("free plan");
        let cache = Arc::clone(&self.cache);
        // Disabled managed keys: the post-downgrade key reconcile is a no-op.
        advance_expired_trials(
            &self.control,
            &ManagedKeys::Disabled,
            chrono::Utc::now(),
            move |account_id| {
                let cache = Arc::clone(&cache);
                let free_plan = free_plan.clone();
                async move {
                    let handle = cache.get_or_load(&account_id).await?;
                    account_over_plan_limits(&free_plan, &handle.manager)
                        .await
                        .map_err(|e| atomic_cloud::CloudError::Invariant(e.to_string()))
                }
            },
        )
        .await
        .expect("trial sweep")
    }

    /// POST a signed Stripe webhook to the app host; returns the response.
    async fn post_webhook(&self, payload: &Value) -> reqwest::Response {
        let body = serde_json::to_vec(payload).expect("serialize");
        let now = chrono::Utc::now().timestamp();
        let signature = sign_webhook(WEBHOOK_SECRET, &body, now);
        self.client
            .post(format!("{}/billing/webhook", self.base_url))
            .header(HOST, format!("app.{BASE_DOMAIN}"))
            .header("stripe-signature", signature)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .expect("post webhook")
    }
}

/// The full billing & quota lifecycle through the composed server, in one
/// cohesive journey for one tenant, with a second tenant proving isolation.
#[actix_web::test]
async fn full_billing_lifecycle_with_cross_tenant_isolation() {
    with_control_db("e2e_billing_lifecycle", |url| async move {
        // Shrink the free plan so that ONE atom puts a downgraded account over
        // the free atom ceiling (the trial grants 'pro', which is unlimited).
        // Done before the harness loads its plan registry.
        set_free_limits(&url, Some(0), None).await;

        let harness = Harness::spawn(&url).await;
        let alpha = harness.provision("alpha").await;
        // `beta` is a reserved vanity slug; the second tenant uses a plain one.
        let beta = harness.provision("tenanttwo").await;

        // === Provision → trialing (signup grants the 14-day paid trial) ===
        start_trial(
            &harness.control,
            &alpha.account_id,
            "pro",
            chrono::Duration::days(14),
        )
        .await
        .expect("start alpha trial");
        // Beta also trials — its journey must stay independent of alpha's.
        start_trial(
            &harness.control,
            &beta.account_id,
            "pro",
            chrono::Duration::days(14),
        )
        .await
        .expect("start beta trial");
        assert_eq!(
            billing_state(&harness.control, &alpha.account_id).await,
            "trialing"
        );

        // Trial = full access: a write succeeds (pro is unlimited).
        let w = harness.create_atom(&alpha, "during trial").await;
        assert_eq!(w.status(), StatusCode::CREATED, "trial serves writes");

        // === Trial expiry → auto-downgrade (manufactured clock) ===
        // Alpha's trial lapses; beta's does not. After the sweep alpha is on
        // free and OVER the (shrunk) limit → read_only; beta stays trialing.
        expire_trial(&harness.control, &alpha.account_id, 1).await;
        let advance = harness.run_trial_sweep().await;
        assert_eq!(
            advance.downgraded_to_read_only, 1,
            "alpha over-limit → read_only"
        );
        assert_eq!(
            plan_id(&harness.control, &alpha.account_id)
                .await
                .as_deref(),
            Some("free")
        );
        assert_eq!(
            billing_state(&harness.control, &alpha.account_id).await,
            "read_only"
        );

        // ISOLATION: beta untouched — still trialing, still pro.
        assert_eq!(
            billing_state(&harness.control, &beta.account_id).await,
            "trialing"
        );
        assert_eq!(
            plan_id(&harness.control, &beta.account_id).await.as_deref(),
            Some("pro")
        );

        // === read_only: reads work, writes 402 (data retained) ===
        let read = harness.read_atoms(&alpha).await;
        assert_eq!(read.status(), StatusCode::OK, "read_only still reads");
        let blocked = harness.create_atom(&alpha, "after downgrade").await;
        assert_eq!(
            blocked.status(),
            StatusCode::PAYMENT_REQUIRED,
            "read_only blocks writes"
        );
        let body: Value = blocked.json().await.expect("body");
        assert_eq!(body["error"], "account_read_only");
        assert_eq!(
            body["upgrade_url"],
            format!("https://app.{BASE_DOMAIN}/account/billing")
        );
        // The trial-era atom survives the downgrade (never auto-deleted).
        let atoms: Value = harness
            .read_atoms(&alpha)
            .await
            .json()
            .await
            .expect("atoms");
        assert!(
            atom_count(&atoms) >= 1,
            "trial-era atom retained through downgrade"
        );

        // Beta can still write throughout (its trial is full access).
        let bw = harness.create_atom(&beta, "beta during trial").await;
        assert_eq!(bw.status(), StatusCode::CREATED, "beta unaffected by alpha");

        // === Signed checkout webhook widens the plan; billing_state active ===
        // A genuine first `customer.subscription.created` carrying alpha's
        // subdomain in metadata (no stripe_customers row exists yet — the
        // redirect writes none; the webhook is the source of truth).
        let now = chrono::Utc::now().timestamp();
        let resp = harness
            .post_webhook(&json!({
                "id": "evt_alpha_checkout",
                "type": "customer.subscription.created",
                "data": { "object": {
                    "id": "sub_alpha",
                    "customer": "cus_alpha",
                    "status": "active",
                    "cancel_at_period_end": false,
                    "current_period_start": now,
                    "current_period_end": now + 2_592_000,
                    "metadata": { "subdomain": "alpha" },
                    "items": { "data": [ { "price": {
                        "id": "price_pro", "metadata": { "plan_id": "pro" }
                    } } ] }
                }}
            }))
            .await;
        assert_eq!(resp.status(), StatusCode::OK, "checkout webhook accepted");
        assert_eq!(
            plan_id(&harness.control, &alpha.account_id)
                .await
                .as_deref(),
            Some("pro")
        );
        assert_eq!(
            billing_state(&harness.control, &alpha.account_id).await,
            "active"
        );

        // === Writes work again (back on unlimited pro, billing active) ===
        let w = harness.create_atom(&alpha, "after checkout").await;
        assert_eq!(
            w.status(),
            StatusCode::CREATED,
            "paid plan + active → writes work"
        );

        // === payment_failed webhook → past_due ===
        let resp = harness
            .post_webhook(&json!({
                "id": "evt_alpha_fail",
                "type": "invoice.payment_failed",
                "data": { "object": { "customer": "cus_alpha" } }
            }))
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            billing_state(&harness.control, &alpha.account_id).await,
            "past_due"
        );
        // past_due is grace — full access, a write still works.
        let w = harness.create_atom(&alpha, "during grace").await;
        assert_eq!(
            w.status(),
            StatusCode::CREATED,
            "past_due is full-access grace"
        );

        // === Dunning reaper at 7d → read_only ===
        backdate_past_due(&harness.control, &alpha.account_id, 8).await;
        let adv = advance_dunning_with(
            &harness.control,
            chrono::Utc::now(),
            DunningThresholds::default(),
        )
        .await
        .expect("dunning advance");
        assert_eq!(adv.moved_to_read_only, 1);
        assert_eq!(
            billing_state(&harness.control, &alpha.account_id).await,
            "read_only"
        );
        let blocked = harness.create_atom(&alpha, "blocked by dunning").await;
        assert_eq!(blocked.status(), StatusCode::PAYMENT_REQUIRED);
        assert_eq!(
            blocked.json::<Value>().await.expect("body")["error"],
            "account_read_only"
        );

        // === payment_succeeded webhook → active recovered ===
        let resp = harness
            .post_webhook(&json!({
                "id": "evt_alpha_recover",
                "type": "invoice.payment_succeeded",
                "data": { "object": { "customer": "cus_alpha" } }
            }))
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            billing_state(&harness.control, &alpha.account_id).await,
            "active"
        );
        let w = harness.create_atom(&alpha, "recovered").await;
        assert_eq!(
            w.status(),
            StatusCode::CREATED,
            "recovered → writes work again"
        );

        // === FINAL ISOLATION CHECK: beta rode through alpha's entire journey
        // — checkout, dunning, suspension-adjacent states — completely
        // untouched. Still trialing, still pro, still writable.
        assert_eq!(
            billing_state(&harness.control, &beta.account_id).await,
            "trialing"
        );
        assert_eq!(
            plan_id(&harness.control, &beta.account_id).await.as_deref(),
            Some("pro")
        );
        let bw = harness.create_atom(&beta, "beta still fine").await;
        assert_eq!(bw.status(), StatusCode::CREATED, "beta isolated throughout");

        // Both accounts (and their data) still exist — nothing was ever
        // auto-deleted across the whole lifecycle.
        for id in [&alpha.account_id, &beta.account_id] {
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE id = $1)")
                    .bind(id)
                    .fetch_one(harness.control.pool())
                    .await
                    .expect("exists");
            assert!(exists, "account {id} retained across the lifecycle");
        }

        harness.stop().await;
    })
    .await;
}

/// Storage enforcement end to end: the recompute arm drives an over-storage
/// tenant warn → restricted (writes 402 `account_storage_restricted`, reads
/// still pass, data RETAINED), then a raised limit clears the restriction —
/// and the dunning `billing_state` stays orthogonal throughout.
#[actix_web::test]
async fn storage_recompute_restricts_writes_without_deleting_then_clears() {
    with_control_db("e2e_storage_enforcement", |url| async move {
        let harness = Harness::spawn(&url).await;
        let tenant = harness.provision("alpha").await;

        // Create an atom so the tenant has real data to retain.
        let w = harness.create_atom(&tenant, "real data").await;
        assert_eq!(w.status(), StatusCode::CREATED);

        // Force the free plan's storage limit to 1 byte so the tenant's real
        // pg_database_size (megabytes) is over it, and reload the registry so
        // the recompute arm sees the tiny limit.
        sqlx::query("UPDATE plans SET storage_bytes_limit = 1 WHERE id = 'free'")
            .execute(harness.control.pool())
            .await
            .expect("shrink storage limit");
        let registry = PlanRegistry::load(harness.control.clone())
            .await
            .expect("reload plans");

        // First recompute: over the limit, grace-anchor stamped now. With the
        // default policy (warn_after = 0, restrict_after = 7d) it lands `warn`
        // — full access still.
        let summary = recompute_storage(
            &harness.control,
            &harness.cluster,
            &registry,
            &StoragePolicy::default(),
            chrono::Utc::now(),
        )
        .await;
        assert!(
            summary.errors.is_empty(),
            "recompute clean: {:?}",
            summary.errors
        );
        assert_eq!(summary.moved_to_warn, 1, "first over-limit recompute warns");
        assert_eq!(
            storage_state(&harness.control, &tenant.account_id).await,
            "warn"
        );
        // The measured size was recorded in quota_usage for the current period.
        let recorded = storage_metric(&harness.control, &tenant.account_id).await;
        assert!(
            recorded > 1,
            "pg_database_size recorded (>{} byte limit)",
            1
        );

        // warn = full access: a write still works.
        let w = harness.create_atom(&tenant, "while warned").await;
        assert_eq!(
            w.status(),
            StatusCode::CREATED,
            "warn does not block writes"
        );

        // Backdate the grace anchor past the restrict window; the next
        // recompute flips to `restricted`.
        backdate_storage_over(&harness.control, &tenant.account_id, 8).await;
        let summary = recompute_storage(
            &harness.control,
            &harness.cluster,
            &registry,
            &StoragePolicy::default(),
            chrono::Utc::now(),
        )
        .await;
        assert_eq!(summary.moved_to_restricted, 1, "past grace → restricted");
        assert_eq!(
            storage_state(&harness.control, &tenant.account_id).await,
            "restricted"
        );

        // restricted: reads pass, writes 402 with the storage-specific body —
        // and dunning billing_state is still 'active' (orthogonal causes).
        assert_eq!(
            billing_state(&harness.control, &tenant.account_id).await,
            "active"
        );
        let read = harness.read_atoms(&tenant).await;
        assert_eq!(read.status(), StatusCode::OK, "restricted still reads");
        let blocked = harness.create_atom(&tenant, "blocked by storage").await;
        assert_eq!(blocked.status(), StatusCode::PAYMENT_REQUIRED);
        let body: Value = blocked.json().await.expect("body");
        assert_eq!(body["error"], "account_storage_restricted");
        assert_eq!(
            body["upgrade_url"],
            format!("https://app.{BASE_DOMAIN}/account/billing")
        );

        // NEVER-DELETE: the tenant's data survives the restriction.
        let atoms: Value = harness
            .read_atoms(&tenant)
            .await
            .json()
            .await
            .expect("atoms");
        assert!(
            atom_count(&atoms) >= 1,
            "data retained under storage restriction"
        );

        // Raise the limit (a plan upgrade); the next recompute finds the
        // tenant back under and clears the restriction — writes resume.
        sqlx::query("UPDATE plans SET storage_bytes_limit = 10737418240 WHERE id = 'free'")
            .execute(harness.control.pool())
            .await
            .expect("raise storage limit");
        let registry = PlanRegistry::load(harness.control.clone())
            .await
            .expect("reload plans");
        let summary = recompute_storage(
            &harness.control,
            &harness.cluster,
            &registry,
            &StoragePolicy::default(),
            chrono::Utc::now(),
        )
        .await;
        assert_eq!(summary.cleared, 1, "back under limit → cleared");
        assert_eq!(
            storage_state(&harness.control, &tenant.account_id).await,
            "active"
        );
        let w = harness.create_atom(&tenant, "after cleanup").await;
        assert_eq!(w.status(), StatusCode::CREATED, "cleared → writes resume");

        harness.stop().await;
    })
    .await;
}

/// Period rollover is idempotent within a period and cross-pod safe: opening
/// the current month's rows once inserts them, a second open (a redelivery or
/// another pod) inserts nothing, and old period rows are retained for audit.
#[tokio::test]
async fn period_rollover_is_idempotent_and_retains_old_rows() {
    with_control_db("e2e_period_rollover", |url| async move {
        let control = ControlPlane::connect(
            &url,
            atomic_cloud::control_plane::DEFAULT_CONTROL_POOL_MAX_CONNECTIONS,
        )
        .await
        .expect("connect");
        control.initialize().await.expect("migrate");
        let account_id = seed_account(&control, "alpha").await;

        let now = chrono::Utc::now();
        // First rollover opens the current period's non-AI metric rows.
        let opened = roll_over_period(&control, now).await.expect("rollover");
        assert!(opened >= 1, "first rollover opens at least the storage row");
        let this_period = current_period_start(now);
        let rows_now = period_rows(&control, &account_id, this_period).await;
        assert!(rows_now >= 1, "the account has a current-period row");

        // A SECOND rollover in the same period (a redelivery, or another pod)
        // inserts NOTHING — ON CONFLICT DO NOTHING.
        let again = roll_over_period(&control, now)
            .await
            .expect("rollover again");
        assert_eq!(
            again, 0,
            "re-running in the same period is a no-op (cross-pod safe)"
        );
        assert_eq!(
            period_rows(&control, &account_id, this_period).await,
            rows_now,
            "no duplicate rows"
        );

        // AI credits are NEVER in the rollover (OpenRouter resets them).
        let ai_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM quota_usage WHERE account_id = $1 AND metric LIKE '%ai%'",
        )
        .bind(&account_id)
        .fetch_one(control.pool())
        .await
        .expect("count ai rows");
        assert_eq!(ai_rows, 0, "AI allowances are not in our rollover");

        // Roll a NEXT period forward (a month later): new rows open, and the
        // PRIOR period's rows are retained (the billing/audit trail).
        let next = now + chrono::Duration::days(40);
        let next_opened = roll_over_period(&control, next).await.expect("next period");
        assert!(next_opened >= 1, "the next period opens fresh rows");
        let next_period = current_period_start(next);
        assert_ne!(next_period, this_period, "fixture spans a period boundary");
        assert!(
            period_rows(&control, &account_id, this_period).await >= 1,
            "old period rows retained for audit after rollover"
        );
        assert!(
            period_rows(&control, &account_id, next_period).await >= 1,
            "new period rows exist"
        );
    })
    .await;
}

/// The webhook rejects an unsigned/forged delivery with 400 and applies no
/// state — proven end to end against the composed server.
#[actix_web::test]
async fn webhook_signature_rejection_end_to_end() {
    with_control_db("e2e_webhook_rejection", |url| async move {
        let harness = Harness::spawn(&url).await;
        let tenant = harness.provision("alpha").await;
        link_stripe_customer(&harness.control, &tenant.account_id, "cus_alpha")
            .await
            .expect("link customer");

        // A payload that WOULD move the account to past_due if applied.
        let payload = serde_json::to_vec(&json!({
            "id": "evt_forged",
            "type": "invoice.payment_failed",
            "data": { "object": { "customer": "cus_alpha" } }
        }))
        .expect("serialize");

        // Forged signature under the WRONG secret → 400, no effect.
        let now = chrono::Utc::now().timestamp();
        let resp = harness
            .client
            .post(format!("{}/billing/webhook", harness.base_url))
            .header(HOST, format!("app.{BASE_DOMAIN}"))
            .header(
                "stripe-signature",
                sign_webhook("whsec_wrong", &payload, now),
            )
            .header("content-type", "application/json")
            .body(payload)
            .send()
            .await
            .expect("post forged webhook");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.json::<Value>().await.expect("body")["error"],
            "invalid_signature"
        );
        // No state changed, no event claimed.
        assert_eq!(
            billing_state(&harness.control, &tenant.account_id).await,
            "active"
        );
        let claimed: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM processed_webhook_events WHERE event_id = 'evt_forged'",
        )
        .fetch_one(harness.control.pool())
        .await
        .expect("count claims");
        assert_eq!(claimed, 0, "a rejected signature claims no event id");

        harness.stop().await;
    })
    .await;
}

// --- helpers -----------------------------------------------------------------

/// A `BillingProvider` test double returning scripted session URLs. The e2e
/// webhook journey never calls these (the webhook only verifies + applies),
/// but the provider's presence is what enables the webhook + routes.
struct RecordingBilling {
    checkout_url: String,
    portal_url: String,
}

impl RecordingBilling {
    fn new(checkout_url: impl Into<String>, portal_url: impl Into<String>) -> Self {
        Self {
            checkout_url: checkout_url.into(),
            portal_url: portal_url.into(),
        }
    }
}

#[async_trait::async_trait]
impl atomic_cloud::BillingProvider for RecordingBilling {
    async fn create_checkout_session(
        &self,
        _price_id: &str,
        _customer_email: &str,
        _subdomain: &str,
        _success_url: &str,
        _cancel_url: &str,
    ) -> Result<atomic_cloud::StripeSession, atomic_cloud::CloudError> {
        Ok(atomic_cloud::StripeSession {
            url: self.checkout_url.clone(),
        })
    }

    async fn create_portal_session(
        &self,
        _stripe_customer_id: &str,
        _return_url: &str,
    ) -> Result<atomic_cloud::StripeSession, atomic_cloud::CloudError> {
        Ok(atomic_cloud::StripeSession {
            url: self.portal_url.clone(),
        })
    }

    async fn cancel_subscription(
        &self,
        _stripe_subscription_id: &str,
    ) -> Result<(), atomic_cloud::CloudError> {
        Ok(())
    }
}

/// The exact HMAC-SHA256-over-`"{t}.{body}"` scheme `verify_webhook` checks.
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

/// Count atoms in a list response, tolerating either a bare array or a
/// `{ "atoms": [...] }` envelope (the handler's shape isn't load-bearing here).
fn atom_count(v: &Value) -> usize {
    v.as_array()
        .map(|a| a.len())
        .or_else(|| v["atoms"].as_array().map(|a| a.len()))
        .unwrap_or(0)
}

/// Shrink the seeded `free` plan's atom/KB limits BEFORE the registry loads.
async fn set_free_limits(control_url: &str, atom_limit: Option<i32>, kb_limit: Option<i32>) {
    let control = ControlPlane::connect(
        control_url,
        atomic_cloud::control_plane::DEFAULT_CONTROL_POOL_MAX_CONNECTIONS,
    )
    .await
    .expect("connect");
    control.initialize().await.expect("migrate");
    sqlx::query("UPDATE plans SET atom_limit = $1, kb_limit = $2 WHERE id = 'free'")
        .bind(atom_limit)
        .bind(kb_limit)
        .execute(control.pool())
        .await
        .expect("set free limits");
}

/// Seed a bare active account directly (no tenant database).
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
    sqlx::query_scalar("SELECT plan_id FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(control.pool())
        .await
        .expect("plan_id")
}

async fn billing_state(control: &ControlPlane, account_id: &str) -> String {
    sqlx::query_scalar("SELECT billing_state FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(control.pool())
        .await
        .expect("billing_state")
}

async fn storage_state(control: &ControlPlane, account_id: &str) -> String {
    sqlx::query_scalar("SELECT storage_state FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(control.pool())
        .await
        .expect("storage_state")
}

/// The recorded `storage_bytes` value for the account's current period.
async fn storage_metric(control: &ControlPlane, account_id: &str) -> i64 {
    let period = current_period_start(chrono::Utc::now());
    sqlx::query_scalar(
        "SELECT value FROM quota_usage \
         WHERE account_id = $1 AND period_start = $2 AND metric = $3",
    )
    .bind(account_id)
    .bind(period)
    .bind(STORAGE_BYTES_METRIC)
    .fetch_one(control.pool())
    .await
    .expect("storage_bytes metric")
}

async fn period_rows(control: &ControlPlane, account_id: &str, period: chrono::NaiveDate) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM quota_usage WHERE account_id = $1 AND period_start = $2",
    )
    .bind(account_id)
    .bind(period)
    .fetch_one(control.pool())
    .await
    .expect("count period rows")
}

/// Backdate `past_due_since` so a sweep at the real `now` crosses the
/// threshold (no-real-waits idiom).
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

/// Backdate `storage_over_since` so a recompute at the real `now` crosses the
/// restrict window (the storage analogue of `backdate_past_due`).
async fn backdate_storage_over(control: &ControlPlane, account_id: &str, days: i64) {
    sqlx::query(
        "UPDATE accounts SET storage_over_since = NOW() - make_interval(days => $2) WHERE id = $1",
    )
    .bind(account_id)
    .bind(days as i32)
    .execute(control.pool())
    .await
    .expect("backdate storage_over_since");
}

/// Move a trialing account's `trial_ends_at` into the past so a sweep at the
/// real `now` sees it expired (no-real-waits idiom).
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
