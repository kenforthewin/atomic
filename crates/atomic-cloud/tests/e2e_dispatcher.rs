//! Slice-wide e2e suite for the dispatcher composition (plan: "Worker
//! fairness & job queue"): the noisy-neighbor scenario end to end, WS event
//! isolation under pooled execution, the streaming-chat semaphore, the
//! paused-tenant hold, and self-hosted parity with the dispatcher off.
//!
//! Each test spawns the real composition — `configure_cloud_app` over an
//! ephemeral port, exactly as `atomic-cloud serve` wires it — in one of
//! three dispatcher modes ([`Mode`]): inline (`--dispatcher=false` parity),
//! manually ticked (deterministic fairness assertions — the tick/settle
//! loop replaces wall-clock waits), or a running `run_loop` (the serve
//! binary's shape, for WS streaming and chat).
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command. Tenant pipelines and chat point at the
//! shared `MockAiServer` — NO REAL PROVIDERS, EVER.

mod support;

use std::sync::Arc;
use std::time::Duration;

use actix_web::{App, HttpServer};
use atomic_cloud::{
    configure_cloud_app, issue_token, provision_account, set_active_provider, upsert_credentials,
    AccountCache, AccountCacheConfig, AccountPlane, AccountPlaneConfig, BreakerConfig,
    ChatStreamLimiter, CloudAuth, ClusterConfig, ControlPlane, CredentialOrigin, Dispatcher,
    DispatcherConfig, FallbackAppState, ManagedKeys, NewAccount, NewCredentials, PoolCaps,
    Provider, SecretKey, TenantPlane, TokenScope, WorkerPoolsConfig,
};
use atomic_test_support::MockAiServer;
use futures_util::StreamExt;
use reqwest::header::HOST;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use sqlx::{Connection, PgConnection};
use support::with_control_db;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const BASE_DOMAIN: &str = "cloudtest.local";
const EVENT_DEADLINE: Duration = Duration::from_secs(20);

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

/// Fast intervals for the loop mode; tests that tick manually ignore the
/// interval fields. Pool caps come from the caller.
fn dispatcher_config(pools: WorkerPoolsConfig, pipeline_batch: i32) -> DispatcherConfig {
    DispatcherConfig {
        tick_interval: Duration::from_millis(100),
        slow_scan_interval: Duration::from_secs(2),
        pipeline_batch_size: pipeline_batch,
        reports_per_tenant_cap: 1,
        pools,
        breaker: BreakerConfig::default(),
        ..DispatcherConfig::default()
    }
}

/// How the harness runs background execution.
enum Mode {
    /// No dispatcher at all and the cache left inline — the
    /// `--dispatcher=false` composition. Tenant saves execute their own
    /// pipelines in-process, exactly like self-hosted.
    Inline,
    /// Dispatcher constructed over the serving cache but never looped;
    /// tests call [`E2eHarness::tick_and_settle`] for deterministic
    /// scheduling assertions.
    ManualTick(DispatcherConfig),
    /// `run_loop` spawned — the serve binary's shape.
    Loop(DispatcherConfig),
}

struct Tenant {
    account_id: String,
    subdomain: String,
    db_name: String,
    token: String,
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The composed cloud server in a chosen dispatcher [`Mode`], plus handles
/// for provisioning, ticking, and inspecting tenants.
struct E2eHarness {
    control: ControlPlane,
    cache: Arc<AccountCache>,
    mock: MockAiServer,
    chat_streams: ChatStreamLimiter,
    dispatcher: Option<Arc<Dispatcher>>,
    dispatcher_loop: Option<tokio::task::JoinHandle<()>>,
    client: reqwest::Client,
    port: u16,
    base_url: String,
    server: actix_web::dev::ServerHandle,
    _fallback: FallbackAppState,
}

impl E2eHarness {
    async fn spawn(control_url: &str, mode: Mode, chat_cap: usize) -> Self {
        let control = connect_control(control_url).await;
        let mock = MockAiServer::start().await;
        let inline = matches!(mode, Mode::Inline);
        let cache = Arc::new(AccountCache::new(
            control.clone(),
            cluster_config(),
            support::test_vault(),
            AccountCacheConfig {
                inline_pipeline: inline,
                // The serve composition installs the deferral policy exactly
                // when a dispatcher runs (main.rs); mirror that here so the
                // harness exercises the production task-run settle path.
                failure_disposition_policy: (!inline).then(|| {
                    atomic_cloud::provider_failure_policy(
                        BreakerConfig::default().credits_recheck,
                        atomic_cloud::DEFAULT_RETRY_AFTER_CAP,
                    )
                }),
                ..AccountCacheConfig::default()
            },
        ));
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
        let chat_streams = ChatStreamLimiter::new(chat_cap);

        // The dispatcher (when any) runs over the SAME cache the server
        // resolves tenants through, so worker events land on the channels
        // live WebSocket clients hold.
        let (dispatcher, dispatcher_loop) = match mode {
            Mode::Inline => (None, None),
            Mode::ManualTick(config) => (
                Some(Arc::new(Dispatcher::new(
                    control.clone(),
                    Arc::clone(&cache),
                    config,
                ))),
                None,
            ),
            Mode::Loop(config) => {
                let dispatcher =
                    Arc::new(Dispatcher::new(control.clone(), Arc::clone(&cache), config));
                let handle = tokio::spawn(Arc::clone(&dispatcher).run_loop());
                (Some(dispatcher), Some(handle))
            }
        };

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local addr").port();
        let state = fallback.data();
        let control_for_app = control.clone();
        let limiter_for_app = chat_streams.clone();
        let server = HttpServer::new(move || {
            App::new().configure(configure_cloud_app(
                state.clone(),
                auth.clone(),
                account_plane.clone(),
                tenant_plane.clone(),
                control_for_app.clone(),
                limiter_for_app.clone(),
            ))
        })
        .workers(1)
        .listen(listener)
        .expect("attach listener")
        .run();
        let handle = server.handle();
        actix_web::rt::spawn(server);

        E2eHarness {
            control,
            cache,
            mock,
            chat_streams,
            dispatcher,
            dispatcher_loop,
            client: reqwest::Client::new(),
            port,
            base_url: format!("http://127.0.0.1:{port}"),
            server: handle,
            _fallback: fallback,
        }
    }

    async fn stop(self) {
        if let Some(handle) = self.dispatcher_loop {
            handle.abort();
        }
        self.server.stop(false).await;
    }

    /// Provision an account with BYOK credentials pointing at the mock AI
    /// server, system tasks disabled (so only the work under test
    /// dispatches), and an account-scope token.
    async fn provision(&self, subdomain: &str) -> Tenant {
        let account = provision_account(
            &self.control,
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

        let tenant = Tenant {
            account_id: account.account_id.clone(),
            subdomain: subdomain.to_string(),
            db_name: account.db_name,
            token: issue_token(
                &self.control,
                &account.account_id,
                TokenScope::Account,
                None,
                "e2e",
            )
            .await
            .expect("issue account token"),
        };
        self.disable_system_tasks(&tenant).await;
        tenant
    }

    /// Disable every system task on the tenant's default knowledge base —
    /// a fresh tenant otherwise always has `draft_pipeline` due, which
    /// would pollute scheduling counts. Written straight into the per-DB
    /// settings tier the scheduler's `is_enabled` gate reads.
    async fn disable_system_tasks(&self, tenant: &Tenant) {
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

    fn api(&self, method: Method, subdomain: &str, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base_url))
            .header(HOST, format!("{subdomain}.{BASE_DOMAIN}"))
    }

    async fn create_atom(&self, tenant: &Tenant, content: &str) -> String {
        let resp = self
            .api(Method::POST, &tenant.subdomain, "/api/atoms")
            .bearer_auth(&tenant.token)
            .json(&json!({ "content": content }))
            .send()
            .await
            .expect("send create atom");
        assert_eq!(resp.status(), StatusCode::CREATED, "create atom");
        let atom: Value = resp.json().await.expect("atom json");
        atom["id"].as_str().expect("atom id").to_string()
    }

    async fn atom_embedding_status(&self, tenant: &Tenant, atom_id: &str) -> String {
        let resp = self
            .api(
                Method::GET,
                &tenant.subdomain,
                &format!("/api/atoms/{atom_id}"),
            )
            .bearer_auth(&tenant.token)
            .send()
            .await
            .expect("send get atom");
        assert_eq!(resp.status(), StatusCode::OK, "atom exists");
        let body: Value = resp.json().await.expect("atom json");
        body["embedding_status"]
            .as_str()
            .expect("embedding_status")
            .to_string()
    }

    /// Poll until the atom's pipeline reaches a terminal state.
    async fn poll_pipeline_done(&self, tenant: &Tenant, atom_id: &str) {
        let deadline = std::time::Instant::now() + EVENT_DEADLINE;
        loop {
            let status = self.atom_embedding_status(tenant, atom_id).await;
            if matches!(status.as_str(), "complete" | "failed" | "skipped") {
                assert_eq!(status, "complete", "pipeline for {atom_id} must succeed");
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "pipeline for {atom_id} not terminal in {EVENT_DEADLINE:?}: {status:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Non-terminal `atom_pipeline_jobs` rows in the tenant's default KB —
    /// the backlog measure for the fairness assertions.
    async fn pipeline_backlog(&self, tenant: &Tenant) -> i32 {
        let core = self
            .cache
            .get_or_load(&tenant.account_id)
            .await
            .expect("load tenant")
            .manager
            .active_core()
            .await
            .expect("active core");
        core.count_pipeline_jobs().await.expect("count jobs")
    }

    /// One dispatcher tick, awaiting every spawned worker, so all ledger
    /// effects are settled when this returns. ManualTick mode only.
    async fn tick_and_settle(&self) -> usize {
        let dispatcher = self
            .dispatcher
            .as_ref()
            .expect("tick_and_settle requires a dispatcher mode");
        let outcome = dispatcher.tick().await;
        let scheduled = outcome.scheduled;
        for handle in outcome.handles {
            handle.await.expect("worker task");
        }
        scheduled
    }

    async fn ws_connect(&self, tenant: &Tenant) -> WsStream {
        let mut request = format!("ws://127.0.0.1:{}/ws", self.port)
            .into_client_request()
            .expect("ws request");
        let headers = request.headers_mut();
        headers.insert(
            "Host",
            format!("{}.{BASE_DOMAIN}", tenant.subdomain)
                .parse()
                .expect("host header"),
        );
        headers.insert(
            "Authorization",
            format!("Bearer {}", tenant.token)
                .parse()
                .expect("auth header"),
        );
        let (ws, _resp) = tokio_tungstenite::connect_async(request)
            .await
            .expect("ws connect");
        ws
    }

    /// Create a conversation and return its id.
    async fn create_conversation(&self, tenant: &Tenant) -> String {
        let resp = self
            .api(Method::POST, &tenant.subdomain, "/api/conversations")
            .bearer_auth(&tenant.token)
            .json(&json!({ "tag_ids": [] }))
            .send()
            .await
            .expect("send create conversation");
        assert_eq!(resp.status(), StatusCode::CREATED, "create conversation");
        let body: Value = resp.json().await.expect("conversation json");
        body["id"].as_str().expect("conversation id").to_string()
    }

    /// A chat-send request for `conversation_id`, ready to send.
    fn chat_send(&self, tenant: &Tenant, conversation_id: &str) -> reqwest::RequestBuilder {
        self.api(
            Method::POST,
            &tenant.subdomain,
            &format!("/api/conversations/{conversation_id}/messages"),
        )
        .bearer_auth(&tenant.token)
        .json(&json!({ "content": "What do my notes say about coffee?" }))
    }
}

/// Read text frames until `predicate` matches one, returning every frame
/// seen (matched frame last).
async fn collect_until<F>(ws: &mut WsStream, deadline: Duration, predicate: F) -> Vec<Value>
where
    F: Fn(&Value) -> bool,
{
    let stop_at = tokio::time::Instant::now() + deadline;
    let mut seen = Vec::new();
    loop {
        let remaining = stop_at
            .checked_duration_since(tokio::time::Instant::now())
            .unwrap_or_else(|| panic!("ws predicate not matched within {deadline:?}: {seen:?}"));
        let msg = tokio::time::timeout(remaining, ws.next())
            .await
            .unwrap_or_else(|_| panic!("ws predicate not matched within {deadline:?}: {seen:?}"))
            .expect("ws stream ended")
            .expect("ws frame");
        match msg {
            Message::Text(t) => {
                let event: Value = serde_json::from_str(&t.to_string()).expect("frame is JSON");
                let matched = predicate(&event);
                seen.push(event);
                if matched {
                    return seen;
                }
            }
            Message::Close(_) => panic!("server closed the ws connection mid-test"),
            _ => {}
        }
    }
}

/// Read whatever text frames arrive within `window` (no predicate; a quiet
/// socket just returns what it has).
async fn drain_frames(ws: &mut WsStream, window: Duration) -> Vec<Value> {
    let stop_at = tokio::time::Instant::now() + window;
    let mut seen = Vec::new();
    loop {
        let remaining = stop_at.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return seen;
        }
        match tokio::time::timeout(remaining, ws.next()).await {
            Err(_elapsed) => return seen,
            Ok(None) => return seen,
            Ok(Some(Ok(Message::Text(t)))) => {
                seen.push(serde_json::from_str(&t.to_string()).expect("frame is JSON"));
            }
            Ok(Some(Ok(_))) | Ok(Some(Err(_))) => continue,
        }
    }
}

fn frames_mention(frames: &[Value], needle: &str) -> bool {
    frames.iter().any(|f| {
        serde_json::to_string(f)
            .expect("serialize frame")
            .contains(needle)
    })
}

// ==================== Tests ====================

/// The noisy-neighbor scenario end to end: tenant A floods 30 atoms through
/// the HTTP API, tenant B creates 2, and per-tenant round-robin fairness
/// means B's pipeline completes while A's backlog is still deep —
/// deterministically, by driving ticks and counting ledger rows, not by
/// wall clock. With per-tenant cap 1 and batch 2, each settled tick
/// executes at most one batch per tenant, so B (one batch) finishes on the
/// first tick while A still holds ≥ 30 − ticks×2 jobs. Both pipelines then
/// run to completion.
#[actix_web::test]
async fn noisy_neighbor_cannot_starve_small_tenant() {
    with_control_db(
        "noisy_neighbor_cannot_starve_small_tenant",
        |url| async move {
            let h = E2eHarness::spawn(
                &url,
                Mode::ManualTick(dispatcher_config(
                    WorkerPoolsConfig {
                        embedding: PoolCaps {
                            total: 2,
                            per_tenant: 1,
                        },
                        ..WorkerPoolsConfig::default()
                    },
                    2, // pipeline batch
                )),
                3,
            )
            .await;
            let noisy = h.provision("noisy").await;
            let quiet = h.provision("quiet").await;

            // The flood, all enqueue-only (dispatcher composition).
            let mut noisy_atoms = Vec::new();
            for i in 0..30 {
                noisy_atoms.push(
                    h.create_atom(&noisy, &format!("noisy backlog note {i} about volume"))
                        .await,
                );
            }
            let quiet_atoms = [
                h.create_atom(&quiet, "quiet note one about espresso").await,
                h.create_atom(&quiet, "quiet note two about pour-over")
                    .await,
            ];
            assert_eq!(h.pipeline_backlog(&noisy).await, 30);
            assert_eq!(h.pipeline_backlog(&quiet).await, 2);

            // Tick until the small tenant drains. Fairness gives it the first
            // tick (its 2 jobs are one batch); the budget only cushions claim
            // hiccups, and the starvation assertion below is what the test is
            // really about.
            let mut ticks = 0;
            while h.pipeline_backlog(&quiet).await > 0 {
                ticks += 1;
                assert!(ticks <= 5, "small tenant still not drained after 5 ticks");
                h.tick_and_settle().await;
            }

            // THE fairness claim: when B is done, A's backlog must still be
            // deep — each settled tick took at most one batch (2 jobs) from A.
            let noisy_left = h.pipeline_backlog(&noisy).await;
            assert!(
                noisy_left >= 30 - ticks * 2,
                "round-robin must not let A jump the line: {noisy_left} left after {ticks} ticks"
            );
            assert!(
                noisy_left > 0,
                "B finishing before A's backlog is the whole point"
            );
            for atom_id in &quiet_atoms {
                assert_eq!(
                    h.atom_embedding_status(&quiet, atom_id).await,
                    "complete",
                    "the small tenant's pipeline must be fully done"
                );
            }

            // And the backlog itself completes — fairness, not denial.
            let mut budget = 40;
            while h.pipeline_backlog(&noisy).await > 0 {
                budget -= 1;
                assert!(budget > 0, "noisy tenant's backlog did not drain");
                h.tick_and_settle().await;
            }
            for atom_id in noisy_atoms.iter().take(3) {
                assert_eq!(h.atom_embedding_status(&noisy, atom_id).await, "complete");
            }

            h.stop().await;
        },
    )
    .await;
}

/// WS event isolation under pooled execution: with the dispatcher loop
/// owning both tenants' pipelines, each tenant's socket streams its own
/// pipeline events and never the other's — the pooled workers publish into
/// per-account channels exactly like the inline path.
#[actix_web::test]
async fn ws_events_stay_tenant_isolated_under_pooled_execution() {
    with_control_db(
        "ws_events_stay_tenant_isolated_under_pooled_execution",
        |url| async move {
            let h = E2eHarness::spawn(
                &url,
                Mode::Loop(dispatcher_config(WorkerPoolsConfig::default(), 8)),
                3,
            )
            .await;
            let alpha = h.provision("alpha").await;
            let bravo = h.provision("bravo").await;

            let mut alpha_ws = h.ws_connect(&alpha).await;
            let mut bravo_ws = h.ws_connect(&bravo).await;

            // Bravo's pipeline first, to completion on bravo's socket: any
            // cross-tenant leak would already be in alpha's buffer.
            let bravo_atom = h.create_atom(&bravo, "bravo note about espresso").await;
            collect_until(&mut bravo_ws, EVENT_DEADLINE, |e| {
                e["type"] == "EmbeddingComplete" && e["atom_id"] == bravo_atom.as_str()
            })
            .await;

            let alpha_atom = h.create_atom(&alpha, "alpha note about pour-over").await;
            let alpha_frames = collect_until(&mut alpha_ws, EVENT_DEADLINE, |e| {
                e["type"] == "EmbeddingComplete" && e["atom_id"] == alpha_atom.as_str()
            })
            .await;

            // Alpha's socket: alpha's whole pipeline lifecycle, none of
            // bravo's — including the frames buffered while bravo ran.
            assert!(
                alpha_frames
                    .iter()
                    .any(|e| e["type"] == "PipelineQueueStarted"),
                "pooled workers must stream the same event family inline \
                 execution does: {alpha_frames:?}"
            );
            assert!(
                !frames_mention(&alpha_frames, &bravo_atom),
                "bravo's atom leaked onto alpha's socket"
            );

            // Bravo's socket saw nothing of alpha's pipeline either.
            let bravo_frames = drain_frames(&mut bravo_ws, Duration::from_millis(500)).await;
            assert!(
                !frames_mention(&bravo_frames, &alpha_atom),
                "alpha's atom leaked onto bravo's socket: {bravo_frames:?}"
            );

            h.stop().await;
        },
    )
    .await;
}

/// The streaming-chat semaphore end to end (plan: "Streaming chat (not in a
/// pool)"): three concurrent streams are fine, the fourth gets the
/// structured 429 while they're in flight, and a finished stream releases
/// its permit so the next send succeeds. Concurrency is held open by
/// MockAiServer's injected chat latency; the in-flight count is polled off
/// the live limiter (the same instance the composition serves with) so the
/// 429 probe never races the three streams' arrival.
#[actix_web::test]
async fn chat_semaphore_caps_concurrent_streams_per_account() {
    with_control_db(
        "chat_semaphore_caps_concurrent_streams_per_account",
        |url| async move {
            let h = E2eHarness::spawn(
                &url,
                Mode::Loop(dispatcher_config(WorkerPoolsConfig::default(), 8)),
                3,
            )
            .await;
            let tenant = h.provision("alpha").await;

            // Four conversations so concurrent sends don't contend on one
            // conversation's message log.
            let mut conversations = Vec::new();
            for _ in 0..4 {
                conversations.push(h.create_conversation(&tenant).await);
            }

            // Hold every chat completion long enough that three sends are
            // provably concurrent.
            h.mock.set_chat_delay(Some(Duration::from_millis(1500)));

            let mut in_flight = tokio::task::JoinSet::new();
            for conversation_id in conversations.iter().take(3) {
                let request = h.chat_send(&tenant, conversation_id);
                in_flight.spawn(async move { request.send().await.expect("send chat") });
            }

            // Deterministic rendezvous: wait until the limiter itself shows
            // all three permits held.
            let deadline = std::time::Instant::now() + EVENT_DEADLINE;
            while h.chat_streams.in_flight(&tenant.account_id) < 3 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "three streams never became concurrent"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            // The fourth stream: structured 429, Retry-After included.
            let denied = h
                .chat_send(&tenant, &conversations[3])
                .send()
                .await
                .expect("send fourth chat");
            assert_eq!(denied.status(), StatusCode::TOO_MANY_REQUESTS);
            assert!(
                denied.headers().get("retry-after").is_some(),
                "429 must carry Retry-After"
            );
            let body: Value = denied.json().await.expect("denial json");
            assert_eq!(body["error"], "too_many_streams");
            assert!(
                body["retry_after_seconds"].as_u64().is_some(),
                "denial must carry a numeric retry hint: {body}"
            );

            // The three streams complete normally — the cap never harms
            // admitted streams.
            while let Some(joined) = in_flight.join_next().await {
                let resp = joined.expect("chat task");
                let status = resp.status();
                let body = resp.text().await.expect("chat body");
                assert_eq!(status, StatusCode::OK, "admitted streams succeed: {body}");
            }

            // Their permits released with their bodies: the fourth
            // conversation's send now goes through.
            h.mock.set_chat_delay(None);
            let deadline = std::time::Instant::now() + EVENT_DEADLINE;
            while h.chat_streams.in_flight(&tenant.account_id) > 0 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "permits not released after streams completed"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let retried = h
                .chat_send(&tenant, &conversations[3])
                .send()
                .await
                .expect("send retried chat");
            assert_eq!(
                retried.status(),
                StatusCode::OK,
                "a finished stream's permit must admit the next send"
            );

            h.stop().await;
        },
    )
    .await;
}

/// The paused-tenant scenario, full e2e: a tenant whose provider pause is
/// in force (the column the circuit breaker writes; the breaker mechanics
/// themselves are pinned in tests/backpressure.rs) has its HTTP-enqueued
/// pipeline work *held* — ledger row intact, atom pending, nothing
/// dispatched — while another tenant proceeds in the same ticks. When the
/// pause lapses, the held work completes without any re-enqueue.
#[actix_web::test]
async fn paused_tenant_work_is_held_then_resumes() {
    with_control_db(
        "paused_tenant_work_is_held_then_resumes",
        |url| async move {
            let h = E2eHarness::spawn(
                &url,
                Mode::ManualTick(dispatcher_config(WorkerPoolsConfig::default(), 8)),
                3,
            )
            .await;
            let paused = h.provision("paused").await;
            let healthy = h.provision("healthy").await;

            sqlx::query(
                "UPDATE accounts SET provider_paused_until = NOW() + interval '1 hour', \
                 provider_pause_kind = 'rate_limit' \
             WHERE id = $1",
            )
            .bind(&paused.account_id)
            .execute(h.control.pool())
            .await
            .expect("pause tenant");

            let paused_atom = h.create_atom(&paused, "held note about patience").await;
            let healthy_atom = h.create_atom(&healthy, "live note about progress").await;

            // Several ticks: the healthy tenant completes, the paused tenant's
            // work sits — pending atom, ledger row intact.
            for _ in 0..3 {
                h.tick_and_settle().await;
            }
            assert_eq!(
                h.atom_embedding_status(&healthy, &healthy_atom).await,
                "complete",
                "the pause must never stall other tenants"
            );
            assert_eq!(
                h.atom_embedding_status(&paused, &paused_atom).await,
                "pending",
                "paused tenant's pipeline must be held, not failed"
            );
            assert_eq!(
                h.pipeline_backlog(&paused).await,
                1,
                "held work sits in the ledger"
            );

            // Pause lapses → the held row dispatches and completes.
            sqlx::query(
                "UPDATE accounts SET provider_paused_until = NOW() - interval '1 second' \
             WHERE id = $1",
            )
            .bind(&paused.account_id)
            .execute(h.control.pool())
            .await
            .expect("unpause tenant");

            let mut budget = 10;
            while h.pipeline_backlog(&paused).await > 0 {
                budget -= 1;
                assert!(
                    budget > 0,
                    "held work did not dispatch after the pause lapsed"
                );
                h.tick_and_settle().await;
            }
            assert_eq!(
                h.atom_embedding_status(&paused, &paused_atom).await,
                "complete"
            );

            h.stop().await;
        },
    )
    .await;
}

/// Self-hosted parity with the dispatcher off (`--dispatcher=false`): the
/// serving process executes pipelines inline exactly as before the
/// dispatcher existed — the atom completes with NO dispatcher ticking, the
/// WS stream carries the same event family, and the ledger ends empty
/// (inline execution claims and clears its own rows). Pins the
/// `inline_pipeline` default against regressions that would strand
/// enqueue-only saves in a dispatcherless process.
#[actix_web::test]
async fn dispatcher_off_preserves_inline_pipeline_behavior() {
    with_control_db(
        "dispatcher_off_preserves_inline_pipeline_behavior",
        |url| async move {
            let h = E2eHarness::spawn(&url, Mode::Inline, 3).await;
            assert!(h.dispatcher.is_none(), "inline mode runs no dispatcher");
            let tenant = h.provision("alpha").await;

            let mut ws = h.ws_connect(&tenant).await;
            let atom_id = h
                .create_atom(&tenant, "inline note about self-hosted parity")
                .await;

            // No ticks anywhere — the serving process itself must finish
            // the pipeline, streaming the same events as always.
            let frames = collect_until(&mut ws, EVENT_DEADLINE, |e| {
                e["type"] == "EmbeddingComplete" && e["atom_id"] == atom_id.as_str()
            })
            .await;
            assert!(
                frames.iter().any(|e| e["type"] == "PipelineQueueStarted"),
                "inline execution must stream the usual queue lifecycle: {frames:?}"
            );
            h.poll_pipeline_done(&tenant, &atom_id).await;
            // The inline runner clears its claimed rows just after the
            // completion event fires; poll briefly rather than racing it.
            let deadline = std::time::Instant::now() + EVENT_DEADLINE;
            while h.pipeline_backlog(&tenant).await > 0 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "inline execution must settle its own ledger rows"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }

            h.stop().await;
        },
    )
    .await;
}

/// A wedged tenant database must not head-of-line-block the tick
/// (`DispatcherConfig::tenant_poll_timeout`). Construction is honest: the
/// wedged tenant's `atom_pipeline_jobs` table is held under an
/// ACCESS EXCLUSIVE lock by an open transaction, so its ledger poll
/// genuinely hangs at the database — exactly the failure mode of a stuck
/// tenant DB — while another tenant's work must still dispatch in the same
/// tick. Releasing the lock lets the next tick pick the skipped tenant
/// back up (its hint was retained, never cleared).
#[actix_web::test]
async fn wedged_tenant_poll_times_out_without_blocking_others() {
    with_control_db(
        "wedged_tenant_poll_times_out_without_blocking_others",
        |url| async move {
            let mut config = dispatcher_config(WorkerPoolsConfig::default(), 8);
            config.tenant_poll_timeout = Duration::from_secs(1);
            let h = E2eHarness::spawn(&url, Mode::ManualTick(config), 3).await;
            let wedged = h.provision("wedged").await;
            let healthy = h.provision("healthy").await;

            let wedged_atom = h
                .create_atom(&wedged, "note stuck behind a wedged database")
                .await;
            let healthy_atom = h
                .create_atom(&healthy, "note that must not wait in line")
                .await;

            // Wedge: hold the table the poll reads first under an exclusive
            // lock for the duration of the tick.
            let wedged_url = cluster_config()
                .tenant_db_url(&wedged.db_name)
                .expect("tenant url");
            let mut lock_conn = PgConnection::connect(&wedged_url)
                .await
                .expect("connect wedged tenant db");
            let mut tx = sqlx::Connection::begin(&mut lock_conn)
                .await
                .expect("open lock transaction");
            sqlx::query("LOCK TABLE atom_pipeline_jobs IN ACCESS EXCLUSIVE MODE")
                .execute(&mut *tx)
                .await
                .expect("lock wedged ledger");

            // The tick must bound the wedged tenant at the poll timeout and
            // still dispatch the healthy tenant's work.
            let started = std::time::Instant::now();
            let scheduled = h.tick_and_settle().await;
            let elapsed = started.elapsed();
            assert!(
                elapsed < Duration::from_secs(8),
                "the wedged tenant must cost at most the poll timeout, not the tick: {elapsed:?}"
            );
            assert_eq!(scheduled, 1, "the healthy tenant's batch must dispatch");
            assert_eq!(
                h.atom_embedding_status(&healthy, &healthy_atom).await,
                "complete",
                "a wedged neighbor must not stall the healthy tenant"
            );
            assert_eq!(
                h.atom_embedding_status(&wedged, &wedged_atom).await,
                "pending",
                "the wedged tenant was skipped, not failed"
            );

            // Lock released → the skipped tenant's retained hint gets it
            // polled again and its work completes.
            tx.rollback().await.expect("release wedge");
            let mut budget = 10;
            while h.pipeline_backlog(&wedged).await > 0 {
                budget -= 1;
                assert!(
                    budget > 0,
                    "wedged tenant did not recover after the lock lifted"
                );
                h.tick_and_settle().await;
            }
            assert_eq!(
                h.atom_embedding_status(&wedged, &wedged_atom).await,
                "complete"
            );

            h.stop().await;
        },
    )
    .await;
}
