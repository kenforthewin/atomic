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
    configure_cloud_app, issue_token, provision_account, set_active_provider, upsert_credentials,
    AccountCache, AccountCacheConfig, AccountPlane, AccountPlaneConfig, BreakerConfig,
    ChatStreamLimiter, CloudAuth, ClusterConfig, ControlPlane, CredentialOrigin, Dispatcher,
    DispatcherConfig, FallbackAppState, ManagedKeys, NewAccount, NewCredentials, Provider,
    ProviderBreaker, SecretKey, TenantPlane, TokenScope, DEFAULT_CHAT_STREAMS_PER_ACCOUNT,
};
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

/// The dispatcher composition's cache shape: inline pipeline OFF, saves are
/// enqueue-only, the dispatcher owns execution.
fn dispatch_cache(control: &ControlPlane) -> Arc<AccountCache> {
    Arc::new(AccountCache::new(
        control.clone(),
        cluster_config(),
        support::test_vault(),
        AccountCacheConfig {
            inline_pipeline: false,
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

/// Disable every system task on the tenant's default knowledge base so only
/// the pipeline rows under test dispatch (same per-DB settings poke as the
/// dispatcher suite).
async fn disable_system_tasks(tenant: &Tenant) {
    let tenant_url = cluster_config()
        .tenant_db_url(&tenant.db_name)
        .expect("tenant url");
    let mut conn = PgConnection::connect(&tenant_url)
        .await
        .expect("connect tenant db");
    for task_id in ["draft_pipeline", "graph_maintenance", "task_runs_gc"] {
        sqlx::query(
            "INSERT INTO settings (db_id, key, value) VALUES ('default', $1, 'false')
             ON CONFLICT (db_id, key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(format!("task.{task_id}.enabled"))
        .execute(&mut conn)
        .await
        .expect("disable task");
    }
    conn.close().await.expect("close");
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
        let control_for_app = control.clone();
        let chat_streams = ChatStreamLimiter::new(DEFAULT_CHAT_STREAMS_PER_ACCOUNT);
        let server = HttpServer::new(move || {
            App::new().configure(configure_cloud_app(
                state.clone(),
                auth.clone(),
                account_plane.clone(),
                tenant_plane.clone(),
                control_for_app.clone(),
                chat_streams.clone(),
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

            // Dispatch resumes: rewind the backed-off rows' not_before (in
            // lieu of waiting out the Retry-After horizon) and tick — the
            // jobs complete against the recovered provider.
            let tenant_url = cluster_config()
                .tenant_db_url(&tenant.db_name)
                .expect("tenant url");
            let mut conn = PgConnection::connect(&tenant_url)
                .await
                .expect("connect tenant db");
            sqlx::query("UPDATE atom_pipeline_jobs SET not_before = '2000-01-01T00:00:00+00:00'")
                .execute(&mut conn)
                .await
                .expect("rewind not_before");
            conn.close().await.expect("close");

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
