//! Provider backpressure tests (plan: "Provider rate-limit handling",
//! "Managed key lifecycle" → "Allowance exhausted", "Live rotation" step 6).
//!
//! Three layers under test, each at its honest seam:
//!
//! - **Layer 1, ledger backoff**: rate-limited pipeline jobs land back in
//!   `atom_pipeline_jobs` with `not_before` honoring the provider's
//!   `Retry-After`, and are not re-dispatched before it.
//! - **Layer 2, the circuit breaker**: trips after exactly the configured
//!   threshold of rate-limit failures (driven through REAL pipeline jobs
//!   against a `MockAiServer` injecting 429s), doubles its cooldown per
//!   consecutive trip, resets after a healthy run, and pauses immediately
//!   on a 402.
//! - **The interactive surface**: a credits pause turns the AI-interactive
//!   routes into the structured `out_of_ai_credits` 402 while atom CRUD
//!   stays fully functional; a rate-limit pause blocks nothing interactive;
//!   a provider mutation (BYOK save) clears the pause and dispatch resumes.
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command. NO REAL PROVIDERS, EVER.

mod support;

use std::sync::Arc;
use std::time::Duration;

use actix_web::{App, HttpServer};
use atomic_cloud::{
    configure_cloud_app, issue_token, provider_failure_policy, provision_account,
    set_active_provider, upsert_credentials, AccountCache, AccountCacheConfig, AccountPlane,
    AccountPlaneConfig, BreakerConfig, ChatStreamLimiter, CloudAuth, ClusterConfig, ControlPlane,
    CredentialOrigin, Dispatcher, DispatcherConfig, FallbackAppState, ManagedKeys, NewAccount,
    NewCredentials, Provider, ProviderBreaker, QuotaBilling, Readiness, SecretKey, TenantPlane,
    TokenScope, DEFAULT_CHAT_STREAMS_PER_ACCOUNT, DEFAULT_RETRY_AFTER_CAP,
};
use atomic_core::models::{TaskRun, TaskRunState};
use atomic_core::wiki::runner::WIKI_REGENERATE_TASK_ID;
use atomic_test_support::{InjectedFailure, MockAiServer};
use chrono::{DateTime, Utc};
use reqwest::header::HOST;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use sqlx::{Connection, PgConnection};
use support::with_control_db;

const BASE_DOMAIN: &str = "cloudtest.local";

fn cluster_config() -> ClusterConfig {
    ClusterConfig {
        cluster_id: "test-cluster-1".to_string(),
        cluster_url: std::env::var("ATOMIC_TEST_DATABASE_URL")
            .expect("with_control_db verified ATOMIC_TEST_DATABASE_URL"),
    }
}

async fn connect_control(control_url: &str) -> ControlPlane {
    let control = ControlPlane::connect(control_url)
        .await
        .expect("connect control plane");
    control.initialize().await.expect("migrate control plane");
    control
}

/// The dispatcher composition's cache shape, exactly as `serve` builds it:
/// inline pipeline OFF (saves are enqueue-only, the dispatcher owns
/// execution) and the provider failure-disposition policy installed, so
/// provider-classified `task_runs` failures defer instead of consuming
/// retry budget.
fn dispatch_cache(control: &ControlPlane) -> Arc<AccountCache> {
    Arc::new(AccountCache::new(
        control.clone(),
        cluster_config(),
        support::test_vault(),
        AccountCacheConfig {
            inline_pipeline: false,
            failure_disposition_policy: Some(provider_failure_policy(
                BreakerConfig::default().credits_recheck,
                DEFAULT_RETRY_AFTER_CAP,
            )),
            ..AccountCacheConfig::default()
        },
    ))
}

struct Tenant {
    account_id: String,
    subdomain: String,
    db_name: String,
}

/// Provision an account with BYOK credentials pointing at `mock`.
async fn provision_tenant(control: &ControlPlane, mock: &MockAiServer, subdomain: &str) -> Tenant {
    let account = provision_account(
        control,
        &cluster_config(),
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
        control,
        vault.as_ref(),
        &account.account_id,
        NewCredentials {
            provider: Provider::OpenAiCompat,
            origin: CredentialOrigin::User,
            api_key: SecretKey::new("test-key".to_string()),
            external_key_id: None,
            model_config: mock_model_config(mock),
        },
    )
    .await
    .expect("store mock provider credentials");
    set_active_provider(
        control,
        &account.account_id,
        Some((Provider::OpenAiCompat, CredentialOrigin::User)),
    )
    .await
    .expect("activate mock provider credentials");

    Tenant {
        account_id: account.account_id,
        subdomain: subdomain.to_string(),
        db_name: account.db_name,
    }
}

fn mock_model_config(mock: &MockAiServer) -> Value {
    json!({
        "embedding_model": "mock-embed",
        "llm_model": "mock-llm",
        "openai_compat_base_url": mock.base_url(),
        "embedding_dimension": 1536,
    })
}

/// Raw connection to the tenant's database — for the settings pokes and
/// ledger inspection the production paths never need.
async fn tenant_conn(tenant: &Tenant) -> PgConnection {
    let tenant_url = cluster_config()
        .tenant_db_url(&tenant.db_name)
        .expect("tenant url");
    PgConnection::connect(&tenant_url)
        .await
        .expect("connect tenant db")
}

/// Upsert one per-DB setting on the tenant's default knowledge base.
async fn set_tenant_setting(tenant: &Tenant, key: &str, value: &str) {
    let mut conn = tenant_conn(tenant).await;
    sqlx::query(
        "INSERT INTO settings (db_id, key, value) VALUES ('default', $1, $2)
         ON CONFLICT (db_id, key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(key)
    .bind(value)
    .execute(&mut conn)
    .await
    .expect("write tenant setting");
    conn.close().await.expect("close");
}

/// Disable every system task on the tenant's default knowledge base so only
/// the pipeline rows under test dispatch (same per-DB settings poke as the
/// dispatcher suite).
async fn disable_system_tasks(tenant: &Tenant) {
    for task_id in ["draft_pipeline", "graph_maintenance", "task_runs_gc"] {
        set_tenant_setting(tenant, &format!("task.{task_id}.enabled"), "false").await;
    }
}

/// Dispatch one REAL maintenance execution (`task_runs_gc` — it never
/// touches the provider) between provider failures: enable it, blank its
/// `last_run` so it's due, tick, disable it again. The issue-1 regression
/// vehicle — pre-fix, each of these successes fed `record_healthy` and
/// reset the breaker's detection window and trip streak.
async fn run_interleaved_maintenance(dispatcher: &Dispatcher, tenant: &Tenant) {
    set_tenant_setting(tenant, "task.task_runs_gc.enabled", "true").await;
    set_tenant_setting(tenant, "task.task_runs_gc.last_run", "").await;
    let scheduled = tick_and_settle(dispatcher).await;
    assert!(
        scheduled >= 1,
        "the interleaved maintenance task must dispatch"
    );
    set_tenant_setting(tenant, "task.task_runs_gc.enabled", "false").await;
}

/// Force the pause horizon into the past — the "cooldown elapsed" clock
/// advance, leaving kind and streak exactly as the breaker wrote them.
async fn expire_pause(control: &ControlPlane, account_id: &str) {
    sqlx::query(
        "UPDATE accounts SET provider_paused_until = NOW() - interval '1 second' WHERE id = $1",
    )
    .bind(account_id)
    .execute(control.pool())
    .await
    .expect("expire pause");
}

/// Pull a `task_runs` row's horizon into the past — simulating the wait-out
/// of a deferral/backoff window so the next tick probes it again.
async fn force_run_due(tenant: &Tenant, run_id: &str) {
    let mut conn = tenant_conn(tenant).await;
    sqlx::query("UPDATE task_runs SET next_attempt_at = $2 WHERE id = $1")
        .bind(run_id)
        .bind((Utc::now() - chrono::Duration::minutes(1)).to_rfc3339())
        .execute(&mut conn)
        .await
        .expect("rewind next_attempt_at");
    conn.close().await.expect("close");
}

/// The tag's single `wiki.regenerate` ledger row.
async fn wiki_regen_run(core: &atomic_core::AtomicCore, tag_id: &str) -> TaskRun {
    let mut runs = core
        .list_task_runs(WIKI_REGENERATE_TASK_ID, Some(tag_id), 10)
        .await
        .expect("list regen runs");
    assert_eq!(runs.len(), 1, "exactly one regen row per tag");
    runs.remove(0)
}

/// Seconds from now until an RFC3339 timestamp (negative when past).
fn secs_until(ts: &str) -> i64 {
    (DateTime::parse_from_rfc3339(ts)
        .expect("rfc3339 timestamp")
        .with_timezone(&Utc)
        - Utc::now())
    .num_seconds()
}

/// The account's pause columns, straight off the control plane.
async fn pause_state(
    control: &ControlPlane,
    account_id: &str,
) -> (Option<DateTime<Utc>>, Option<String>, i32) {
    sqlx::query_as(
        "SELECT provider_paused_until, provider_pause_kind, provider_pause_streak \
         FROM accounts WHERE id = $1",
    )
    .bind(account_id)
    .fetch_one(control.pool())
    .await
    .expect("read pause state")
}

/// One dispatcher tick, awaiting every spawned worker so all ledger and
/// breaker effects are settled when this returns.
async fn tick_and_settle(dispatcher: &Dispatcher) -> usize {
    let outcome = dispatcher.tick().await;
    let scheduled = outcome.scheduled;
    for handle in outcome.handles {
        handle.await.expect("worker task");
    }
    scheduled
}

/// Enqueue one atom through an inline-off core (the dispatcher
/// composition's save path) and return its id.
async fn enqueue_atom(core: &atomic_core::AtomicCore, content: &str) -> String {
    core.create_atom(
        atomic_core::CreateAtomRequest {
            content: content.to_string(),
            ..Default::default()
        },
        |_| {},
    )
    .await
    .expect("create atom")
    .expect("atom inserted")
    .atom
    .id
}

// ==================== Breaker: trips on 3, not 2 + ledger backoff ===========

/// The detection threshold through REAL pipeline jobs: two rate-limited
/// executions do NOT pause the tenant; the third does. Along the way, layer
/// 1 is pinned: each failed job is re-enqueued with `not_before` honoring
/// the injected `Retry-After`, sits in the ledger (total count) without
/// being claimable (due count), and an immediate re-tick dispatches nothing.
#[tokio::test]
async fn breaker_trips_after_three_rate_limited_failures_not_two() {
    with_control_db(
        "breaker_trips_after_three_rate_limited_failures_not_two",
        |url| async move {
            let control = connect_control(&url).await;
            let mock = MockAiServer::start().await;
            let tenant = provision_tenant(&control, &mock, "alpha").await;
            disable_system_tasks(&tenant).await;

            let cache = dispatch_cache(&control);
            let dispatcher = Dispatcher::new(
                control.clone(),
                Arc::clone(&cache),
                DispatcherConfig {
                    tick_interval: Duration::from_millis(100),
                    pipeline_batch_size: 1,
                    ..DispatcherConfig::default()
                },
            );
            let core = cache
                .get_or_load(&tenant.account_id)
                .await
                .expect("load tenant")
                .manager
                .active_core()
                .await
                .expect("active core");

            // Every embedding call 429s with a long Retry-After, so failed
            // jobs back off far past this test's lifetime — re-dispatch
            // can't race the assertions.
            mock.set_embedding_failure(Some(InjectedFailure::RateLimited {
                retry_after_secs: Some(300),
            }));

            // Two failures: noted, NOT tripped.
            enqueue_atom(&core, "note one about rate limits").await;
            enqueue_atom(&core, "note two about rate limits").await;
            let scheduled = tick_and_settle(&dispatcher).await;
            assert_eq!(scheduled, 2, "both jobs dispatch as separate batches");

            let (paused_until, kind, streak) = pause_state(&control, &tenant.account_id).await;
            assert!(
                paused_until.is_none() && kind.is_none() && streak == 0,
                "two rate-limited failures must NOT trip the breaker \
                 (paused_until={paused_until:?}, kind={kind:?}, streak={streak})"
            );

            // Layer 1: both jobs were re-enqueued with not_before honoring
            // Retry-After — present in the ledger, not claimable, and an
            // immediate re-tick dispatches nothing.
            assert_eq!(core.count_pipeline_jobs().await.expect("count"), 2);
            assert_eq!(core.count_due_pipeline_jobs().await.expect("due"), 0);
            let tenant_url = cluster_config()
                .tenant_db_url(&tenant.db_name)
                .expect("tenant url");
            let mut conn = PgConnection::connect(&tenant_url)
                .await
                .expect("connect tenant db");
            let backed_off: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM atom_pipeline_jobs \
                 WHERE state = 'pending' AND not_before > $1 AND reason = 'provider-backoff'",
            )
            .bind(Utc::now().to_rfc3339())
            .fetch_one(&mut conn)
            .await
            .expect("count backed-off rows");
            conn.close().await.expect("close");
            assert_eq!(
                backed_off, 2,
                "both rate-limited jobs must sit pending with a future not_before"
            );
            assert_eq!(
                tick_and_settle(&dispatcher).await,
                0,
                "backed-off jobs must not re-dispatch before not_before"
            );

            // Third failure: trip. Streak 1, kind rate_limit, future pause.
            enqueue_atom(&core, "note three about rate limits").await;
            let scheduled = tick_and_settle(&dispatcher).await;
            assert_eq!(scheduled, 1, "only the fresh job is due");

            let (paused_until, kind, streak) = pause_state(&control, &tenant.account_id).await;
            let paused_until = paused_until.expect("third rate-limited failure must trip");
            assert!(paused_until > Utc::now(), "pause must be in the future");
            assert_eq!(kind.as_deref(), Some("rate_limit"));
            assert_eq!(streak, 1);

            // And the paused tenant is skipped wholesale on the next tick.
            assert_eq!(
                tick_and_settle(&dispatcher).await,
                0,
                "a paused tenant must not dispatch"
            );
        },
    )
    .await;
}

// ==================== Breaker: cooldown doubling + healthy reset ============

/// The cooldown schedule, driven deterministically against the breaker
/// itself (real control plane, manufactured failures): trip 1 pauses for
/// the base cooldown, trip 2 for double, a healthy run resets the streak,
/// and the next trip is back at the base.
#[tokio::test]
async fn breaker_cooldown_doubles_per_trip_and_resets_after_healthy_run() {
    with_control_db(
        "breaker_cooldown_doubles_per_trip_and_resets_after_healthy_run",
        |url| async move {
            let control = connect_control(&url).await;
            let mock = MockAiServer::start().await;
            let tenant = provision_tenant(&control, &mock, "alpha").await;
            let breaker = ProviderBreaker::new(control.clone(), BreakerConfig::default());

            // Cooldown bounds are generous: the pause is computed from the
            // database's NOW(), compared against this process's clock.
            let trip = |n: u32| {
                let breaker = &breaker;
                let account_id = tenant.account_id.clone();
                async move {
                    for i in 0..2 {
                        let noted = breaker
                            .record_rate_limited(&account_id)
                            .await
                            .expect("record failure");
                        assert!(noted.is_none(), "trip {n}: failure {i} must not trip early");
                    }
                    breaker
                        .record_rate_limited(&account_id)
                        .await
                        .expect("record failure")
                        .unwrap_or_else(|| panic!("trip {n}: third failure must trip"))
                }
            };
            let cooldown_secs = |until: DateTime<Utc>| (until - Utc::now()).num_seconds();

            // Trip 1: ~60s.
            let until = trip(1).await;
            let secs = cooldown_secs(until);
            assert!(
                (30..=90).contains(&secs),
                "trip 1 cooldown ~60s, got {secs}s"
            );
            let (_, _, streak) = pause_state(&control, &tenant.account_id).await;
            assert_eq!(streak, 1);

            // Trip 2 (consecutive): ~120s.
            let until = trip(2).await;
            let secs = cooldown_secs(until);
            assert!(
                (90..=150).contains(&secs),
                "trip 2 cooldown ~120s, got {secs}s"
            );
            let (_, _, streak) = pause_state(&control, &tenant.account_id).await;
            assert_eq!(streak, 2);

            // Healthy run: streak resets...
            breaker
                .record_healthy(&tenant.account_id)
                .await
                .expect("record healthy");
            let (_, _, streak) = pause_state(&control, &tenant.account_id).await;
            assert_eq!(streak, 0, "a healthy run must reset the streak");

            // ...so the next trip is back at the base cooldown.
            let until = trip(3).await;
            let secs = cooldown_secs(until);
            assert!(
                (30..=90).contains(&secs),
                "post-reset trip cooldown ~60s, got {secs}s"
            );
        },
    )
    .await;
}

// ==================== HTTP harness for the interactive surface ==============

/// The serve-shaped composition (HTTP server exactly as `configure_cloud_app`
/// wires it) plus a manually-ticked dispatcher over the same cache.
struct Harness {
    control: ControlPlane,
    mock: MockAiServer,
    dispatcher: Dispatcher,
    cache: Arc<AccountCache>,
    client: reqwest::Client,
    base_url: String,
    server: actix_web::dev::ServerHandle,
    _fallback: FallbackAppState,
}

impl Harness {
    async fn spawn(control_url: &str) -> Self {
        let control = connect_control(control_url).await;
        let mock = MockAiServer::start().await;
        let cache = dispatch_cache(&control);
        let auth = CloudAuth::new(control.clone(), Arc::clone(&cache), BASE_DOMAIN);
        let account_plane = AccountPlane::new(
            control.clone(),
            cluster_config(),
            ManagedKeys::Disabled,
            Arc::new(support::CapturingSender::default()),
            AccountPlaneConfig::new(BASE_DOMAIN),
        )
        .expect("build account plane");
        let tenant_plane = TenantPlane::new(
            control.clone(),
            cluster_config(),
            ManagedKeys::Disabled,
            support::test_vault(),
            Arc::clone(&cache),
        );
        let fallback = FallbackAppState::build().expect("build fallback state");

        // Manual ticks (no run_loop): every test step is deterministic.
        let dispatcher = Dispatcher::new(
            control.clone(),
            Arc::clone(&cache),
            DispatcherConfig {
                pipeline_batch_size: 1,
                ..DispatcherConfig::default()
            },
        );

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

        Harness {
            control,
            mock,
            dispatcher,
            cache,
            client: reqwest::Client::new(),
            base_url: format!("http://127.0.0.1:{port}"),
            server: handle,
            _fallback: fallback,
        }
    }

    async fn stop(self) {
        self.server.stop(false).await;
    }

    fn api(&self, method: Method, subdomain: &str, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base_url))
            .header(HOST, format!("{subdomain}.{BASE_DOMAIN}"))
    }

    async fn tenant_core(&self, tenant: &Tenant) -> atomic_core::AtomicCore {
        self.cache
            .get_or_load(&tenant.account_id)
            .await
            .expect("load tenant")
            .manager
            .active_core()
            .await
            .expect("active core")
    }

    /// Create a tag and one tagged atom over HTTP, then drive the pipeline
    /// to completion against the (healthy) mock so wiki generation has
    /// embedded sources. Returns the tag id.
    async fn seed_wiki_sources(&self, tenant: &Tenant, token: &str, tag_name: &str) -> String {
        let resp = self
            .api(Method::POST, &tenant.subdomain, "/api/tags")
            .bearer_auth(token)
            .json(&json!({ "name": tag_name, "parent_id": null }))
            .send()
            .await
            .expect("create tag");
        assert_eq!(resp.status(), StatusCode::CREATED, "tag create");
        let tag: Value = resp.json().await.expect("tag json");
        let tag_id = tag["id"].as_str().expect("tag id").to_string();

        let resp = self
            .api(Method::POST, &tenant.subdomain, "/api/atoms")
            .bearer_auth(token)
            .json(&json!({
                "content": "Notes about wave functions and superposition for the wiki.",
                "tag_ids": [tag_id],
            }))
            .send()
            .await
            .expect("create atom");
        assert_eq!(resp.status(), StatusCode::CREATED, "atom create");
        let atom: Value = resp.json().await.expect("atom json");
        let atom_id = atom["id"].as_str().expect("atom id").to_string();

        let core = self.tenant_core(tenant).await;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            tick_and_settle(&self.dispatcher).await;
            let status = core
                .get_atom(&atom_id)
                .await
                .expect("get atom")
                .expect("atom exists")
                .atom
                .embedding_status;
            if status == "complete" {
                break;
            }
            assert_ne!(status, "failed", "seed pipeline must not fail");
            assert!(
                std::time::Instant::now() < deadline,
                "seed pipeline did not finish (status {status:?})"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        tag_id
    }
}

// ==================== 402 → credits pause → interactive guard ===============

/// The allowance-exhausted path end to end: a provider 402 during a
/// dispatched pipeline job pauses the tenant with `kind = 'credits'`, the
/// job sits in the ledger, the interactive chat route answers the
/// structured `out_of_ai_credits` 402 — and atom creation keeps working
/// (plan: "Atoms still create/update fine").
#[actix_web::test]
async fn credit_exhaustion_pauses_and_guards_interactive_routes() {
    with_control_db(
        "credit_exhaustion_pauses_and_guards_interactive_routes",
        |url| async move {
            let h = Harness::spawn(&url).await;
            let tenant = provision_tenant(&h.control, &h.mock, "alpha").await;
            disable_system_tasks(&tenant).await;
            let token = issue_token(
                &h.control,
                &tenant.account_id,
                TokenScope::Account,
                None,
                "e2e",
            )
            .await
            .expect("issue token");

            // Atom CRUD works before the pause (and the save is
            // enqueue-only — the job is the 402 vehicle below).
            h.mock
                .set_embedding_failure(Some(InjectedFailure::PaymentRequired));
            let resp = h
                .api(Method::POST, &tenant.subdomain, "/api/atoms")
                .bearer_auth(&token)
                .json(&json!({ "content": "A note that will hit the credit wall." }))
                .send()
                .await
                .expect("create atom");
            assert_eq!(resp.status(), StatusCode::CREATED);

            // Dispatch the job; the 402 pauses the tenant as credits.
            let scheduled = tick_and_settle(&h.dispatcher).await;
            assert_eq!(scheduled, 1, "the pipeline job must dispatch once");
            let (paused_until, kind, streak) = pause_state(&h.control, &tenant.account_id).await;
            let paused_until = paused_until.expect("402 must pause the tenant");
            assert_eq!(kind.as_deref(), Some("credits"));
            assert_eq!(
                streak, 0,
                "a credits pause must not touch the rate-limit streak"
            );
            let horizon_secs = (paused_until - Utc::now()).num_seconds();
            assert!(
                (3000..=4200).contains(&horizon_secs),
                "credits pause uses the ~1h recheck horizon, got {horizon_secs}s"
            );

            // The job sits in the ledger (blocked, not failed-and-gone).
            let core = h.tenant_core(&tenant).await;
            assert_eq!(
                core.count_pipeline_jobs().await.expect("count"),
                1,
                "the credit-blocked job must sit in the ledger"
            );

            // Interactive chat: the structured 402.
            let resp = h
                .api(
                    Method::POST,
                    &tenant.subdomain,
                    "/api/conversations/any-conversation/messages",
                )
                .bearer_auth(&token)
                .json(&json!({ "content": "hello?" }))
                .send()
                .await
                .expect("send chat message");
            assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
            let body: Value = resp.json().await.expect("error body");
            assert_eq!(body["error"], "out_of_ai_credits");
            assert_eq!(
                body["resets_at"],
                paused_until.to_rfc3339(),
                "resets_at must carry the pause horizon"
            );
            assert_eq!(
                body["upgrade_url"],
                format!("https://app.{BASE_DOMAIN}/billing"),
                "upgrade_url placeholder derives from the tenant's base domain"
            );

            // Wiki generation is guarded the same way.
            let resp = h
                .api(
                    Method::POST,
                    &tenant.subdomain,
                    "/api/wiki/some-tag/generate",
                )
                .bearer_auth(&token)
                .send()
                .await
                .expect("generate wiki");
            assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);

            // Atom CRUD stays fully functional while paused.
            let resp = h
                .api(Method::POST, &tenant.subdomain, "/api/atoms")
                .bearer_auth(&token)
                .json(&json!({ "content": "Notes still work while out of credits." }))
                .send()
                .await
                .expect("create atom while paused");
            assert_eq!(
                resp.status(),
                StatusCode::CREATED,
                "atom creation must keep working under a credits pause"
            );
            let resp = h
                .api(Method::GET, &tenant.subdomain, "/api/atoms")
                .bearer_auth(&token)
                .send()
                .await
                .expect("list atoms while paused");
            assert_eq!(resp.status(), StatusCode::OK);

            h.stop().await;
        },
    )
    .await;
}

// ==================== Rate-limit pause does NOT block interactive ===========

/// The asymmetry: a `rate_limit` pause governs background dispatch only —
/// the interactive routes pass the guard untouched (here: the chat route
/// proceeds into its handler and fails on the missing conversation, never
/// with `out_of_ai_credits`).
#[actix_web::test]
async fn rate_limit_pause_does_not_block_interactive_routes() {
    with_control_db(
        "rate_limit_pause_does_not_block_interactive_routes",
        |url| async move {
            let h = Harness::spawn(&url).await;
            let tenant = provision_tenant(&h.control, &h.mock, "alpha").await;
            let token = issue_token(
                &h.control,
                &tenant.account_id,
                TokenScope::Account,
                None,
                "e2e",
            )
            .await
            .expect("issue token");

            sqlx::query(
                "UPDATE accounts SET provider_paused_until = NOW() + interval '1 hour', \
                     provider_pause_kind = 'rate_limit', provider_pause_streak = 1 \
                 WHERE id = $1",
            )
            .bind(&tenant.account_id)
            .execute(h.control.pool())
            .await
            .expect("pause tenant as rate_limit");

            let resp = h
                .api(
                    Method::POST,
                    &tenant.subdomain,
                    "/api/conversations/no-such-conversation/messages",
                )
                .bearer_auth(&token)
                .json(&json!({ "content": "still here?" }))
                .send()
                .await
                .expect("send chat message");
            assert_ne!(
                resp.status(),
                StatusCode::PAYMENT_REQUIRED,
                "a rate-limit pause must not block interactive routes"
            );
            let body: Value = resp.json().await.expect("body");
            assert_ne!(body["error"], "out_of_ai_credits");

            h.stop().await;
        },
    )
    .await;
}

// ==================== Provider mutation clears the pause ====================

/// Rotation step 6, end to end: trip the breaker through real rate-limited
/// pipeline work, rotate to a fresh BYOK key over HTTP, and assert the
/// pause + streak cleared in the same transaction as the generation bump —
/// then watch dispatch resume and the backed-off job complete against the
/// recovered provider.
#[actix_web::test]
async fn provider_mutation_clears_pause_and_dispatch_resumes() {
    with_control_db(
        "provider_mutation_clears_pause_and_dispatch_resumes",
        |url| async move {
            let h = Harness::spawn(&url).await;
            let tenant = provision_tenant(&h.control, &h.mock, "alpha").await;
            disable_system_tasks(&tenant).await;
            let token = issue_token(
                &h.control,
                &tenant.account_id,
                TokenScope::Account,
                None,
                "e2e",
            )
            .await
            .expect("issue token");
            let core = h.tenant_core(&tenant).await;

            // Trip the breaker with three rate-limited pipeline executions.
            h.mock
                .set_embedding_failure(Some(InjectedFailure::RateLimited {
                    retry_after_secs: Some(300),
                }));
            for i in 0..3 {
                enqueue_atom(&core, &format!("note {i} that keeps getting limited")).await;
            }
            let mut budget = 5;
            loop {
                tick_and_settle(&h.dispatcher).await;
                let (paused_until, _, _) = pause_state(&h.control, &tenant.account_id).await;
                if paused_until.is_some() {
                    break;
                }
                budget -= 1;
                assert!(budget > 0, "three rate-limited jobs must trip the breaker");
            }
            let (paused_until, kind, streak) = pause_state(&h.control, &tenant.account_id).await;
            assert!(paused_until.expect("tripped") > Utc::now());
            assert_eq!(kind.as_deref(), Some("rate_limit"));
            assert_eq!(streak, 1);
            let generation_before: i64 =
                sqlx::query_scalar("SELECT provider_generation FROM accounts WHERE id = $1")
                    .bind(&tenant.account_id)
                    .fetch_one(h.control.pool())
                    .await
                    .expect("read generation");

            // The provider "recovers" and the user rotates to a fresh key
            // through the real BYOK route (validation embeds against the
            // healthy mock before anything is stored).
            h.mock.set_embedding_failure(None);
            let resp = h
                .api(Method::PUT, &tenant.subdomain, "/api/account/provider")
                .bearer_auth(&token)
                .json(&json!({
                    "provider": "openai_compat",
                    "api_key": "rotated-key",
                    "model_config": mock_model_config(&h.mock),
                }))
                .send()
                .await
                .expect("rotate BYOK key");
            assert_eq!(resp.status(), StatusCode::OK);

            // Pause + streak cleared, generation bumped — one transaction.
            let (paused_until, kind, streak) = pause_state(&h.control, &tenant.account_id).await;
            assert!(
                paused_until.is_none() && kind.is_none(),
                "a provider mutation must clear the pause \
                 (paused_until={paused_until:?}, kind={kind:?})"
            );
            assert_eq!(streak, 0, "a provider mutation must reset the streak");
            let generation_after: i64 =
                sqlx::query_scalar("SELECT provider_generation FROM accounts WHERE id = $1")
                    .bind(&tenant.account_id)
                    .fetch_one(h.control.pool())
                    .await
                    .expect("read generation");
            assert!(
                generation_after > generation_before,
                "the pause clear rides the generation bump"
            );

            // Dispatch resumes through the REAL recovery path: the mutation
            // also re-armed every `provider-backoff` row's not_before to
            // "now" (no manual rewind), so the backed-off jobs are due
            // immediately instead of waiting out their 300s Retry-After.
            assert_eq!(
                core.count_due_pipeline_jobs().await.expect("due"),
                3,
                "the provider mutation must re-arm every backed-off pipeline row"
            );

            let mut budget = 10;
            while core.count_pipeline_jobs().await.expect("count") > 0 {
                let scheduled = tick_and_settle(&h.dispatcher).await;
                assert!(
                    scheduled > 0 || core.count_pipeline_jobs().await.expect("count") == 0,
                    "an unpaused tenant with due work must dispatch"
                );
                budget -= 1;
                assert!(budget > 0, "pipeline ledger did not drain after rotation");
            }
            // A healthy run also resets in-memory detection state; the
            // streak stays 0.
            let (_, _, streak) = pause_state(&h.control, &tenant.account_id).await;
            assert_eq!(streak, 0);

            h.stop().await;
        },
    )
    .await;
}

// ==================== Breaker: only provider work feeds detection ===========

/// Only provider-touching executions feed the breaker. A chronically
/// rate-limited tenant whose failures are interleaved with healthy
/// maintenance runs (`task_runs_gc` never calls the provider) still trips
/// at exactly the threshold, and the cooldown keeps escalating
/// 60s → 120s → 240s across consecutive trips. Pre-fix, every maintenance
/// success called `record_healthy`: the detection window never reached
/// threshold and the streak reset between trips, so a chronic-429 tenant
/// with any background housekeeping never escalated (or never even
/// tripped).
#[tokio::test]
async fn breaker_escalates_across_trips_despite_interleaved_maintenance() {
    with_control_db(
        "breaker_escalates_despite_interleaved_maintenance",
        |url| async move {
            let control = connect_control(&url).await;
            let mock = MockAiServer::start().await;
            let tenant = provision_tenant(&control, &mock, "alpha").await;
            disable_system_tasks(&tenant).await;

            let cache = dispatch_cache(&control);
            let dispatcher = Dispatcher::new(
                control.clone(),
                Arc::clone(&cache),
                DispatcherConfig {
                    tick_interval: Duration::from_millis(100),
                    pipeline_batch_size: 1,
                    ..DispatcherConfig::default()
                },
            );
            let core = cache
                .get_or_load(&tenant.account_id)
                .await
                .expect("load tenant")
                .manager
                .active_core()
                .await
                .expect("active core");

            mock.set_embedding_failure(Some(InjectedFailure::RateLimited {
                retry_after_secs: Some(300),
            }));

            let mut atom_n = 0usize;
            for (trip, band) in [(1i32, 30..=90i64), (2, 90..=150), (3, 210..=270)] {
                for failure in 0..3 {
                    if failure > 0 {
                        // A REAL provider-free execution between failures.
                        run_interleaved_maintenance(&dispatcher, &tenant).await;
                        let (paused_until, _, streak) =
                            pause_state(&control, &tenant.account_id).await;
                        assert!(
                            paused_until.is_none_or(|t| t <= Utc::now()),
                            "trip {trip}: maintenance must not pause the tenant"
                        );
                        assert_eq!(
                            streak,
                            trip - 1,
                            "trip {trip}: a maintenance success must not reset the trip streak"
                        );
                    }
                    atom_n += 1;
                    enqueue_atom(&core, &format!("rate-limited note {atom_n}")).await;
                    assert_eq!(
                        tick_and_settle(&dispatcher).await,
                        1,
                        "trip {trip}: exactly the fresh job dispatches"
                    );
                }

                let (paused_until, kind, streak) = pause_state(&control, &tenant.account_id).await;
                let paused_until = paused_until
                    .unwrap_or_else(|| panic!("trip {trip}: the third failure must trip"));
                assert!(paused_until > Utc::now(), "trip {trip}: future pause");
                assert_eq!(kind.as_deref(), Some("rate_limit"));
                assert_eq!(streak, trip, "trip {trip}: streak escalates per trip");
                let secs = (paused_until - Utc::now()).num_seconds();
                assert!(
                    band.contains(&secs),
                    "trip {trip}: cooldown must escalate despite interleaved \
                     maintenance (expected {band:?}, got {secs}s)"
                );

                // The cooldown "elapses" so the next trip's failures dispatch.
                expire_pause(&control, &tenant.account_id).await;
            }
        },
    )
    .await;
}

/// The flip side of the gating: a GENUINE provider-class success still
/// resets detection. Two rate-limited failures, then one healthy pipeline
/// execution, then two more failures — no trip, because the success
/// cleared the window (without the reset, the third failure overall would
/// have tripped). The third post-reset failure trips.
#[tokio::test]
async fn provider_class_success_genuinely_resets_breaker_detection() {
    with_control_db(
        "provider_class_success_resets_breaker_detection",
        |url| async move {
            let control = connect_control(&url).await;
            let mock = MockAiServer::start().await;
            let tenant = provision_tenant(&control, &mock, "alpha").await;
            disable_system_tasks(&tenant).await;

            let cache = dispatch_cache(&control);
            let dispatcher = Dispatcher::new(
                control.clone(),
                Arc::clone(&cache),
                DispatcherConfig {
                    tick_interval: Duration::from_millis(100),
                    pipeline_batch_size: 1,
                    ..DispatcherConfig::default()
                },
            );
            let core = cache
                .get_or_load(&tenant.account_id)
                .await
                .expect("load tenant")
                .manager
                .active_core()
                .await
                .expect("active core");

            let fail_once = |n: usize| {
                let dispatcher = &dispatcher;
                let core = &core;
                async move {
                    enqueue_atom(core, &format!("rate-limited note {n}")).await;
                    assert_eq!(tick_and_settle(dispatcher).await, 1);
                }
            };

            mock.set_embedding_failure(Some(InjectedFailure::RateLimited {
                retry_after_secs: Some(300),
            }));
            fail_once(1).await;
            fail_once(2).await;

            // One healthy provider-class execution.
            mock.set_embedding_failure(None);
            enqueue_atom(&core, "healthy note about recovery").await;
            assert_eq!(tick_and_settle(&dispatcher).await, 1);

            // Two more failures: still below threshold — the success reset
            // the window. (Without the reset, the first of these would have
            // been the window's third failure and tripped.)
            mock.set_embedding_failure(Some(InjectedFailure::RateLimited {
                retry_after_secs: Some(300),
            }));
            fail_once(3).await;
            fail_once(4).await;
            let (paused_until, _, _) = pause_state(&control, &tenant.account_id).await;
            assert!(
                paused_until.is_none(),
                "a provider-class success must genuinely reset the detection window"
            );

            // The third post-reset failure trips.
            fail_once(5).await;
            let (paused_until, kind, streak) = pause_state(&control, &tenant.account_id).await;
            assert!(paused_until.expect("third post-reset failure trips") > Utc::now());
            assert_eq!(kind.as_deref(), Some("rate_limit"));
            assert_eq!(streak, 1);
        },
    )
    .await;
}

// ==================== task_runs deferral: 402 / 429 =========================

/// Plan: "Allowance exhausted → jobs sit in the ledger as blocked". A wiki
/// regeneration hitting the provider's credit wall DEFERS its `task_runs`
/// row — back to pending at the credits-recheck horizon, lease released,
/// retry budget untouched — instead of consuming `max_attempts` (3) and
/// terminally abandoning the regen, which a month-long exhaustion would
/// otherwise do in three probes. Recovery is the real mechanism end to
/// end: the BYOK rotation clears the pause AND re-arms the deferred
/// horizon, and the next tick regenerates.
#[actix_web::test]
async fn credit_exhausted_wiki_regen_defers_without_burning_attempts() {
    with_control_db("wiki_regen_defers_on_credit_exhaustion", |url| async move {
        let h = Harness::spawn(&url).await;
        let tenant = provision_tenant(&h.control, &h.mock, "alpha").await;
        disable_system_tasks(&tenant).await;
        let token = issue_token(
            &h.control,
            &tenant.account_id,
            TokenScope::Account,
            None,
            "e2e",
        )
        .await
        .expect("issue token");
        let core = h.tenant_core(&tenant).await;
        let tag_id = h.seed_wiki_sources(&tenant, &token, "Quantum Notes").await;

        let assert_deferred = |run: &TaskRun, label: &str| {
            assert_eq!(
                run.state,
                TaskRunState::Pending,
                "{label}: deferred — pending, never failed/abandoned"
            );
            assert_eq!(
                run.attempts, 0,
                "{label}: deferral refunds the claim's attempt — retry budget untouched"
            );
            assert!(run.lease_until.is_none(), "{label}: lease released");
            let horizon = secs_until(&run.next_attempt_at);
            assert!(
                (3000..=4000).contains(&horizon),
                "{label}: horizon at the credits recheck (~1h), got {horizon}s"
            );
            assert!(
                run.last_error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("API error (402)"),
                "{label}: the provider failure is recorded as last_error"
            );
        };

        // The credit wall goes up; the user requests a wiki. The route
        // fails (500), but the ledger row defers through the installed
        // policy — same core, same policy, transport-independent.
        h.mock
            .set_chat_failure(Some(InjectedFailure::PaymentRequired));
        let resp = h
            .api(
                Method::POST,
                &tenant.subdomain,
                &format!("/api/wiki/{tag_id}/generate"),
            )
            .bearer_auth(&token)
            .json(&json!({ "tag_name": "Quantum Notes" }))
            .send()
            .await
            .expect("generate wiki");
        assert!(
            !resp.status().is_success(),
            "generation against the credit wall must fail"
        );
        let run = wiki_regen_run(&core, &tag_id).await;
        assert_deferred(&run, "after the route-inline failure");
        let run_id = run.id;

        // A month of exhaustion: more probes than the whole retry
        // budget. Each deferral horizon "passes", the dispatcher
        // re-probes, the 402 re-defers. Pre-fix, probe 3 would have
        // abandoned the row for good.
        for probe in 1..=4 {
            expire_pause(&h.control, &tenant.account_id).await;
            force_run_due(&tenant, &run_id).await;
            tick_and_settle(&h.dispatcher).await;
            let run = wiki_regen_run(&core, &tag_id).await;
            assert_eq!(run.id, run_id, "probe {probe}: same row, never re-created");
            assert_deferred(&run, &format!("probe {probe}"));
        }
        // The dispatched 402s also paused the tenant as credits.
        let (paused_until, kind, _) = pause_state(&h.control, &tenant.account_id).await;
        assert!(paused_until.expect("dispatched 402 must pause") > Utc::now());
        assert_eq!(kind.as_deref(), Some("credits"));

        // Billing fixed → the rotation clears the pause AND re-arms the
        // deferred horizon to "now" (the real recovery, no manual
        // rewind)...
        h.mock.set_chat_failure(None);
        let resp = h
            .api(Method::PUT, &tenant.subdomain, "/api/account/provider")
            .bearer_auth(&token)
            .json(&json!({
                "provider": "openai_compat",
                "api_key": "rotated-key",
                "model_config": mock_model_config(&h.mock),
            }))
            .send()
            .await
            .expect("rotate BYOK key");
        assert_eq!(resp.status(), StatusCode::OK);
        let (paused_until, kind, _) = pause_state(&h.control, &tenant.account_id).await;
        assert!(
            paused_until.is_none() && kind.is_none(),
            "the rotation clears the pause"
        );
        let run = wiki_regen_run(&core, &tag_id).await;
        assert_eq!(run.state, TaskRunState::Pending);
        assert!(
            secs_until(&run.next_attempt_at) <= 1,
            "the rotation must re-arm the deferred horizon to now"
        );

        // ...and the next tick regenerates with the full budget intact.
        let scheduled = tick_and_settle(&h.dispatcher).await;
        assert!(scheduled >= 1, "the re-armed regen dispatches immediately");
        let run = wiki_regen_run(&core, &tag_id).await;
        assert_eq!(run.state, TaskRunState::Succeeded);
        assert_eq!(
            run.attempts, 1,
            "the successful run consumed exactly its own claim"
        );
        assert!(run.result_id.is_some(), "the article landed");

        h.stop().await;
    })
    .await;
}

/// Layer 1 for the task ledger, made literal: a 429's `Retry-After` lands
/// in `task_runs.next_attempt_at` (the plan's "record the rate-limit-reset
/// header"), clamped to the configured cap against hostile hints — and the
/// pipeline ledger's re-enqueued `not_before` clamps the same way.
#[actix_web::test]
async fn rate_limit_retry_after_lands_in_ledger_horizons_clamped() {
    with_control_db(
        "rate_limit_retry_after_lands_in_ledger_horizons",
        |url| async move {
            let h = Harness::spawn(&url).await;
            let tenant = provision_tenant(&h.control, &h.mock, "alpha").await;
            disable_system_tasks(&tenant).await;
            let token = issue_token(
                &h.control,
                &tenant.account_id,
                TokenScope::Account,
                None,
                "e2e",
            )
            .await
            .expect("issue token");
            let core = h.tenant_core(&tenant).await;
            let tag_id = h.seed_wiki_sources(&tenant, &token, "Tides Notes").await;

            // Retry-After honored: 120s lands in next_attempt_at.
            h.mock.set_chat_failure(Some(InjectedFailure::RateLimited {
                retry_after_secs: Some(120),
            }));
            let resp = h
                .api(
                    Method::POST,
                    &tenant.subdomain,
                    &format!("/api/wiki/{tag_id}/generate"),
                )
                .bearer_auth(&token)
                .json(&json!({ "tag_name": "Tides Notes" }))
                .send()
                .await
                .expect("generate wiki");
            assert!(!resp.status().is_success());
            let run = wiki_regen_run(&core, &tag_id).await;
            assert_eq!(run.state, TaskRunState::Pending);
            assert_eq!(run.attempts, 0, "a rate-limit deferral never burns budget");
            let horizon = secs_until(&run.next_attempt_at);
            assert!(
                (100..=140).contains(&horizon),
                "Retry-After must land in next_attempt_at, got {horizon}s"
            );
            // One 429 must not pause (threshold is 3) — the deferred
            // horizon alone holds the row.
            let (paused_until, _, _) = pause_state(&h.control, &tenant.account_id).await;
            assert!(paused_until.is_none());

            // A hostile hint clamps to the cap (default 15 min).
            h.mock.set_chat_failure(Some(InjectedFailure::RateLimited {
                retry_after_secs: Some(86_400),
            }));
            force_run_due(&tenant, &run.id).await;
            tick_and_settle(&h.dispatcher).await;
            let run = wiki_regen_run(&core, &tag_id).await;
            assert_eq!(run.attempts, 0);
            let horizon = secs_until(&run.next_attempt_at);
            assert!(
                (840..=960).contains(&horizon),
                "a hostile Retry-After must clamp to the cap, got {horizon}s"
            );

            // The pipeline ledger's re-enqueue clamps the same hint into
            // not_before.
            h.mock
                .set_embedding_failure(Some(InjectedFailure::RateLimited {
                    retry_after_secs: Some(86_400),
                }));
            let resp = h
                .api(Method::POST, &tenant.subdomain, "/api/atoms")
                .bearer_auth(&token)
                .json(&json!({ "content": "A note behind a hostile rate limit." }))
                .send()
                .await
                .expect("create atom");
            assert_eq!(resp.status(), StatusCode::CREATED);
            let atom: Value = resp.json().await.expect("atom json");
            let atom_id = atom["id"].as_str().expect("atom id").to_string();
            tick_and_settle(&h.dispatcher).await;

            let mut conn = tenant_conn(&tenant).await;
            let (not_before, reason): (String, String) = sqlx::query_as(
                "SELECT not_before, reason FROM atom_pipeline_jobs WHERE atom_id = $1",
            )
            .bind(&atom_id)
            .fetch_one(&mut conn)
            .await
            .expect("re-enqueued pipeline row");
            conn.close().await.expect("close");
            assert_eq!(reason, "provider-backoff");
            let horizon = secs_until(&not_before);
            assert!(
                (840..=960).contains(&horizon),
                "the pipeline not_before must clamp the same hint, got {horizon}s"
            );

            h.stop().await;
        },
    )
    .await;
}

// ==================== 401/403 → provider-kind pause ==========================

/// Credential rejections (the plan's "BYOK key expired" breaker case) enter
/// the pause machinery as `kind = 'provider'`: background dispatch holds,
/// the blocked job sits at the pause horizon, interactive routes stay
/// un-gated (the user's own action gets the provider's real auth error) —
/// and a key rotation clears the pause, re-arms the blocked row, and
/// dispatch resumes within one tick.
#[actix_web::test]
async fn auth_failure_pauses_as_provider_kind_and_rotation_recovers() {
    with_control_db("auth_failure_pauses_as_provider_kind", |url| async move {
        let h = Harness::spawn(&url).await;
        let tenant = provision_tenant(&h.control, &h.mock, "alpha").await;
        disable_system_tasks(&tenant).await;
        let token = issue_token(
            &h.control,
            &tenant.account_id,
            TokenScope::Account,
            None,
            "e2e",
        )
        .await
        .expect("issue token");
        let core = h.tenant_core(&tenant).await;

        // The key "expires": every embedding call rejects with 401.
        h.mock
            .set_embedding_failure(Some(InjectedFailure::Unauthorized));
        let resp = h
            .api(Method::POST, &tenant.subdomain, "/api/atoms")
            .bearer_auth(&token)
            .json(&json!({ "content": "A note behind an expired API key." }))
            .send()
            .await
            .expect("create atom");
        assert_eq!(resp.status(), StatusCode::CREATED);
        let atom: Value = resp.json().await.expect("atom json");
        let atom_id = atom["id"].as_str().expect("atom id").to_string();
        assert_eq!(tick_and_settle(&h.dispatcher).await, 1);

        let (paused_until, kind, streak) = pause_state(&h.control, &tenant.account_id).await;
        let paused_until = paused_until.expect("a credential rejection must pause immediately");
        assert_eq!(kind.as_deref(), Some("provider"));
        assert_eq!(streak, 0, "auth pauses never touch the rate-limit streak");
        let horizon = (paused_until - Utc::now()).num_seconds();
        assert!(
            (3000..=4200).contains(&horizon),
            "the provider pause re-probes on the recheck horizon, got {horizon}s"
        );

        // The job sits blocked at the pause horizon — not failed-and-gone.
        assert_eq!(core.count_pipeline_jobs().await.expect("count"), 1);
        assert_eq!(core.count_due_pipeline_jobs().await.expect("due"), 0);
        assert_eq!(
            tick_and_settle(&h.dispatcher).await,
            0,
            "a provider-paused tenant must not dispatch"
        );

        // Unlike credits, the interactive guard stays open — the chat
        // route proceeds into its handler (and fails on the missing
        // conversation), never with the structured credits error.
        let resp = h
            .api(
                Method::POST,
                &tenant.subdomain,
                "/api/conversations/no-such-conversation/messages",
            )
            .bearer_auth(&token)
            .json(&json!({ "content": "still here?" }))
            .send()
            .await
            .expect("send chat message");
        assert_ne!(resp.status(), StatusCode::PAYMENT_REQUIRED);
        let body: Value = resp.json().await.expect("body");
        assert_ne!(body["error"], "out_of_ai_credits");

        // Rotation (validated against the healed mock) clears the pause,
        // re-arms the blocked row, and the next tick drains it.
        h.mock.set_embedding_failure(None);
        let resp = h
            .api(Method::PUT, &tenant.subdomain, "/api/account/provider")
            .bearer_auth(&token)
            .json(&json!({
                "provider": "openai_compat",
                "api_key": "rotated-key",
                "model_config": mock_model_config(&h.mock),
            }))
            .send()
            .await
            .expect("rotate BYOK key");
        assert_eq!(resp.status(), StatusCode::OK);
        let (paused_until, kind, _) = pause_state(&h.control, &tenant.account_id).await;
        assert!(
            paused_until.is_none() && kind.is_none(),
            "the rotation must clear the provider pause"
        );
        assert_eq!(
            core.count_due_pipeline_jobs().await.expect("due"),
            1,
            "the rotation must re-arm the blocked row to now"
        );
        assert_eq!(
            tick_and_settle(&h.dispatcher).await,
            1,
            "dispatch resumes within one tick of the rotation"
        );
        let atom = core
            .get_atom(&atom_id)
            .await
            .expect("get atom")
            .expect("atom exists");
        assert_eq!(atom.atom.embedding_status, "complete");

        h.stop().await;
    })
    .await;
}
