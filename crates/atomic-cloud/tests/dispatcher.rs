//! Dispatcher integration tests (plan: "Worker fairness & job queue").
//!
//! Two styles, per the testing conventions:
//!
//! - **Deterministic scheduling tests** drive [`Dispatcher::drain`] directly
//!   with manufactured per-tenant queues and a recording executor, so
//!   round-robin fairness and the pool caps are asserted without wall-clock
//!   dependence beyond worker sleep durations.
//! - **Ledger tests** run real ticks against provisioned tenants: claim
//!   exclusivity across two dispatcher instances ("pods"), crash-lease
//!   reclaim, the hint lifecycle, the pause gate, and the full HTTP→WS
//!   pipeline e2e with the dispatcher owning execution.
//!
//! Postgres-gated; see `tests/support/mod.rs` for the skip/cleanup
//! conventions and the run command. Tenant pipelines that execute for real
//! point at the shared `MockAiServer` — NO REAL PROVIDERS, EVER.

mod support;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use actix_web::{App, HttpServer};
use atomic_cloud::{
    configure_cloud_app, issue_token, list_hinted_accounts, mark_hint, provision_account,
    set_active_provider, upsert_credentials, AccountCache, AccountCacheConfig, AccountPlane,
    AccountPlaneConfig, CloudAuth, CloudError, ClusterConfig, ControlPlane, CoreExecutor,
    CredentialOrigin, Dispatcher, DispatcherConfig, ExecOutcome, FallbackAppState, ManagedKeys,
    NewAccount, NewCredentials, PoolCaps, Provider, SecretKey, TenantPlane, TenantQueue,
    TokenScope, WorkClass, WorkExecutor, WorkItem, WorkerPoolsConfig,
};
use atomic_core::models::{TaskRunState, TaskRunTrigger};
use atomic_core::{DatabaseManager, TaskRun};
use atomic_test_support::MockAiServer;
use chrono::Utc;
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

/// The cache shape the dispatcher composition uses: inline pipeline
/// execution OFF, so tenant saves are enqueue-only and the dispatcher owns
/// execution.
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

/// Small pools + fast intervals for direct-tick tests. Individual tests
/// override the caps they exercise.
fn test_config(pools: WorkerPoolsConfig) -> DispatcherConfig {
    DispatcherConfig {
        tick_interval: Duration::from_millis(100),
        slow_scan_interval: Duration::from_secs(60),
        pipeline_batch_size: 4,
        reports_per_tenant_cap: 1,
        pools,
    }
}

struct Tenant {
    account_id: String,
    subdomain: String,
    db_name: String,
}

/// Provision an account. When `mock` is given, store BYOK credentials
/// pointing at it (real pipeline executions); otherwise the account runs
/// keyless (fine for system tasks, which make no provider calls).
async fn provision_tenant(
    control: &ControlPlane,
    mock: Option<&MockAiServer>,
    subdomain: &str,
) -> Tenant {
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

    if let Some(mock) = mock {
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
            control,
            &account.account_id,
            Some((Provider::OpenAiCompat, CredentialOrigin::User)),
        )
        .await
        .expect("activate mock provider credentials");
    }

    Tenant {
        account_id: account.account_id,
        subdomain: subdomain.to_string(),
        db_name: account.db_name,
    }
}

/// Open the tenant's database directly (separate from any AccountCache).
async fn tenant_manager(tenant: &Tenant) -> DatabaseManager {
    let url = cluster_config()
        .tenant_db_url(&tenant.db_name)
        .expect("tenant url");
    DatabaseManager::new_postgres(".", &url)
        .await
        .expect("open tenant manager")
}

/// Disable every system task on the tenant's default knowledge base so the
/// tenant is genuinely idle (a fresh tenant otherwise always has
/// `draft_pipeline` due, which would keep its hint alive forever). Written
/// straight into the per-DB settings tier the scheduler's `is_enabled`
/// gate reads (the facade's `set_setting` targets the `'_global'` tier on
/// Postgres; the per-DB writer is crate-internal).
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

// ==================== Executor seams ====================

#[derive(Default)]
struct RecorderState {
    in_flight_tenant: HashMap<(WorkClass, String), usize>,
    in_flight_kind: HashMap<(String, String), usize>,
    in_flight_total: HashMap<WorkClass, usize>,
    max_tenant: HashMap<(WorkClass, String), usize>,
    max_kind: HashMap<(String, String), usize>,
    max_total: HashMap<WorkClass, usize>,
    /// `(account_id, item key)` in completion order.
    completions: Vec<(String, String)>,
}

fn item_key(item: &WorkItem) -> String {
    match item {
        WorkItem::PipelineBatch { .. } => "pipeline".to_string(),
        WorkItem::SystemTask { task_id, .. } => format!("task:{task_id}"),
        WorkItem::FeedPoll { .. } => "feed".to_string(),
        WorkItem::WikiRegen { .. } => "wiki".to_string(),
        WorkItem::Report { .. } => "report".to_string(),
    }
}

/// Fake executor: records per-(class, tenant), per-(tenant, kind), and
/// per-class concurrency high-water marks plus completion order; "executes"
/// by sleeping `delay`. Never touches a database.
struct RecordingExecutor {
    delay: Duration,
    state: Mutex<RecorderState>,
}

impl RecordingExecutor {
    fn new(delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            delay,
            state: Mutex::new(RecorderState::default()),
        })
    }

    fn completions(&self) -> Vec<(String, String)> {
        self.state.lock().unwrap().completions.clone()
    }

    fn max_tenant(&self, class: WorkClass, tenant: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .max_tenant
            .get(&(class, tenant.to_string()))
            .copied()
            .unwrap_or(0)
    }

    fn max_kind(&self, tenant: &str, kind: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .max_kind
            .get(&(tenant.to_string(), kind.to_string()))
            .copied()
            .unwrap_or(0)
    }

    fn max_total(&self, class: WorkClass) -> usize {
        self.state
            .lock()
            .unwrap()
            .max_total
            .get(&class)
            .copied()
            .unwrap_or(0)
    }
}

#[async_trait::async_trait]
impl WorkExecutor for RecordingExecutor {
    async fn execute(&self, account_id: &str, item: &WorkItem) -> Result<ExecOutcome, CloudError> {
        let class = item.class();
        let kind = item_key(item);
        {
            let mut s = self.state.lock().unwrap();
            let t = s
                .in_flight_tenant
                .entry((class, account_id.to_string()))
                .or_insert(0);
            *t += 1;
            let t = *t;
            let k = s
                .in_flight_kind
                .entry((account_id.to_string(), kind.clone()))
                .or_insert(0);
            *k += 1;
            let k = *k;
            let c = s.in_flight_total.entry(class).or_insert(0);
            *c += 1;
            let c = *c;
            let mt = s
                .max_tenant
                .entry((class, account_id.to_string()))
                .or_insert(0);
            *mt = (*mt).max(t);
            let mk = s
                .max_kind
                .entry((account_id.to_string(), kind.clone()))
                .or_insert(0);
            *mk = (*mk).max(k);
            let mc = s.max_total.entry(class).or_insert(0);
            *mc = (*mc).max(c);
        }

        tokio::time::sleep(self.delay).await;

        {
            let mut s = self.state.lock().unwrap();
            *s.in_flight_tenant
                .get_mut(&(class, account_id.to_string()))
                .unwrap() -= 1;
            *s.in_flight_kind
                .get_mut(&(account_id.to_string(), kind.clone()))
                .unwrap() -= 1;
            *s.in_flight_total.get_mut(&class).unwrap() -= 1;
            s.completions.push((account_id.to_string(), kind));
        }
        Ok(ExecOutcome::Executed)
    }
}

/// Wraps a real executor and records which items actually *executed*
/// (claim won and ran) vs were skipped — the execution-counting seam the
/// double-dispatch and reclaim tests assert on.
struct CountingExecutor {
    inner: Arc<dyn WorkExecutor>,
    executed: Mutex<Vec<(String, String)>>,
}

impl CountingExecutor {
    fn new(inner: Arc<dyn WorkExecutor>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            executed: Mutex::new(Vec::new()),
        })
    }

    fn executed(&self) -> Vec<(String, String)> {
        self.executed.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl WorkExecutor for CountingExecutor {
    async fn execute(&self, account_id: &str, item: &WorkItem) -> Result<ExecOutcome, CloudError> {
        let outcome = self.inner.execute(account_id, item).await?;
        if matches!(outcome, ExecOutcome::Executed) {
            self.executed
                .lock()
                .unwrap()
                .push((account_id.to_string(), item_key(item)));
        }
        Ok(outcome)
    }
}

fn pipeline_items(n: usize) -> VecDeque<WorkItem> {
    (0..n)
        .map(|_| WorkItem::PipelineBatch {
            db_id: "default".to_string(),
            batch: 1,
        })
        .collect()
}

fn fake_wiki_run(subject: &str) -> TaskRun {
    let now = Utc::now().to_rfc3339();
    TaskRun {
        id: uuid::Uuid::new_v4().to_string(),
        task_id: "wiki.regenerate".to_string(),
        subject_id: Some(subject.to_string()),
        state: TaskRunState::Pending,
        trigger: TaskRunTrigger::Schedule,
        attempts: 0,
        max_attempts: 3,
        lease_until: None,
        next_attempt_at: now.clone(),
        scope: None,
        result_id: None,
        last_error: None,
        started_at: None,
        finished_at: None,
        created_at: now.clone(),
        updated_at: now,
    }
}

/// Drive `drain` to completion: each round admits what the pools allow,
/// awaits that round's workers, and repeats until the queues are empty.
/// Awaiting between rounds makes the completion order deterministic.
async fn drain_in_rounds(dispatcher: &Dispatcher, queues: &mut VecDeque<TenantQueue>) {
    let mut budget = 1000;
    while !queues.is_empty() {
        budget -= 1;
        assert!(budget > 0, "drain did not converge");
        let (_, handles) = dispatcher.drain(queues).await;
        for handle in handles {
            handle.await.expect("worker task");
        }
    }
}

// ==================== Deterministic scheduling tests ====================

/// Round-robin fairness: tenant A has a 50-item backlog, tenant B has 2
/// items; with per-tenant cap 1 and total 2, B's work completes while A's
/// backlog has barely started — the backlog cannot starve the small tenant.
#[tokio::test]
async fn round_robin_interleaves_tenants_fairly() {
    with_control_db("round_robin_interleaves_tenants_fairly", |url| async move {
        let control = connect_control(&url).await;
        let recorder = RecordingExecutor::new(Duration::from_millis(20));
        let dispatcher = Dispatcher::with_executor(
            control.clone(),
            dispatch_cache(&control),
            test_config(WorkerPoolsConfig {
                embedding: PoolCaps {
                    total: 2,
                    per_tenant: 1,
                },
                ..WorkerPoolsConfig::default()
            }),
            recorder.clone(),
        )
        .await
        .expect("build dispatcher");

        let mut queues: VecDeque<TenantQueue> = VecDeque::from([
            TenantQueue {
                account_id: "tenant-a".to_string(),
                items: pipeline_items(50),
            },
            TenantQueue {
                account_id: "tenant-b".to_string(),
                items: pipeline_items(2),
            },
        ]);
        drain_in_rounds(&dispatcher, &mut queues).await;

        let completions = recorder.completions();
        assert_eq!(completions.len(), 52, "every item executed");
        let b_last = completions
            .iter()
            .rposition(|(acct, _)| acct == "tenant-b")
            .expect("tenant-b executed");
        let a_fifth = completions
            .iter()
            .enumerate()
            .filter(|(_, (acct, _))| acct == "tenant-a")
            .map(|(i, _)| i)
            .nth(4)
            .expect("tenant-a executed at least 5 items");
        assert!(
            b_last < a_fifth,
            "tenant B (2 items) must finish before tenant A's 5th of 50 \
             (B last at {b_last}, A 5th at {a_fifth}): {completions:?}"
        );
    })
    .await;
}

/// Per-tenant and total caps hold under load: concurrency high-water marks
/// measured inside the executor never exceed the configured caps, while
/// real parallelism still happens.
#[tokio::test]
async fn pool_caps_respected_under_load() {
    with_control_db("pool_caps_respected_under_load", |url| async move {
        let control = connect_control(&url).await;
        let recorder = RecordingExecutor::new(Duration::from_millis(40));
        let dispatcher = Dispatcher::with_executor(
            control.clone(),
            dispatch_cache(&control),
            test_config(WorkerPoolsConfig {
                embedding: PoolCaps {
                    total: 3,
                    per_tenant: 2,
                },
                ..WorkerPoolsConfig::default()
            }),
            recorder.clone(),
        )
        .await
        .expect("build dispatcher");

        let mut queues: VecDeque<TenantQueue> = VecDeque::from([
            TenantQueue {
                account_id: "tenant-a".to_string(),
                items: pipeline_items(10),
            },
            TenantQueue {
                account_id: "tenant-b".to_string(),
                items: pipeline_items(10),
            },
        ]);

        // Re-drain on a short cadence WITHOUT awaiting rounds, so admission
        // happens while workers are mid-flight — the load shape that would
        // expose a cap leak.
        let mut handles = Vec::new();
        let mut budget = 1000;
        while !queues.is_empty() {
            budget -= 1;
            assert!(budget > 0, "drain did not converge");
            let (_, mut spawned) = dispatcher.drain(&mut queues).await;
            handles.append(&mut spawned);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        for handle in handles {
            handle.await.expect("worker task");
        }

        assert_eq!(recorder.completions().len(), 20);
        for tenant in ["tenant-a", "tenant-b"] {
            let max = recorder.max_tenant(WorkClass::Embedding, tenant);
            assert!(max <= 2, "{tenant} exceeded per-tenant cap: {max}");
        }
        let max_total = recorder.max_total(WorkClass::Embedding);
        assert!(max_total <= 3, "class total cap exceeded: {max_total}");
        assert!(
            max_total >= 2,
            "expected real parallelism under the total cap, saw {max_total}"
        );
    })
    .await;
}

/// The reports work-type override: with the llm class allowing 2 in flight
/// per tenant, two report runs for one tenant still serialize (cap 1),
/// while a wiki-regen item shares the class concurrently.
#[tokio::test]
async fn reports_per_tenant_override_serializes_reports() {
    with_control_db(
        "reports_per_tenant_override_serializes_reports",
        |url| async move {
            let control = connect_control(&url).await;
            let recorder = RecordingExecutor::new(Duration::from_millis(40));
            let dispatcher = Dispatcher::with_executor(
                control.clone(),
                dispatch_cache(&control),
                test_config(WorkerPoolsConfig::default()), // llm: 16 total / 2 per-tenant
                recorder.clone(),
            )
            .await
            .expect("build dispatcher");

            let items: VecDeque<WorkItem> = VecDeque::from([
                WorkItem::Report {
                    db_id: "default".to_string(),
                    report_id: "r1".to_string(),
                },
                WorkItem::Report {
                    db_id: "default".to_string(),
                    report_id: "r2".to_string(),
                },
                WorkItem::WikiRegen {
                    db_id: "default".to_string(),
                    run: Box::new(fake_wiki_run("tag-1")),
                },
            ]);
            let mut queues = VecDeque::from([TenantQueue {
                account_id: "tenant-a".to_string(),
                items,
            }]);

            let mut handles = Vec::new();
            let mut budget = 1000;
            while !queues.is_empty() {
                budget -= 1;
                assert!(budget > 0, "drain did not converge");
                let (_, mut spawned) = dispatcher.drain(&mut queues).await;
                handles.append(&mut spawned);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            for handle in handles {
                handle.await.expect("worker task");
            }

            assert_eq!(recorder.completions().len(), 3, "all three items ran");
            assert_eq!(
                recorder.max_kind("tenant-a", "report"),
                1,
                "two reports for one tenant must serialize"
            );
            assert!(
                recorder.max_total(WorkClass::Llm) >= 2,
                "the override must not tighten the class itself: a wiki \
                 regen should run alongside a report"
            );
        },
    )
    .await;
}

// ==================== Ledger / claim tests ====================

/// Two dispatcher instances ("pods") over the same control plane and
/// tenant double-process nothing: six pipeline-ledger rows are enqueued up
/// front, both pods tick concurrently with small claim batches, and every
/// row executes exactly once — pinned by counting each atom's
/// `EmbeddingComplete` event across both pods' tenant channels (the
/// `FOR UPDATE SKIP LOCKED` claim is the only thing standing between the
/// pods and a double embed).
#[tokio::test]
async fn two_dispatchers_execute_each_ledger_row_once() {
    with_control_db(
        "two_dispatchers_execute_each_ledger_row_once",
        |url| async move {
            let control = connect_control(&url).await;
            let mock = MockAiServer::start().await;
            let tenant = provision_tenant(&control, Some(&mock), "alpha").await;
            // Only the pipeline rows under test should dispatch.
            disable_system_tasks(&tenant).await;

            let mut dispatchers = Vec::new();
            let mut receivers = Vec::new();
            for _ in 0..2 {
                let cache = dispatch_cache(&control);
                // Subscribe to this pod's tenant event channel before any
                // work runs — workers resolve the same cache entry.
                let handle = cache
                    .get_or_load(&tenant.account_id)
                    .await
                    .expect("load tenant");
                receivers.push(handle.event_tx.subscribe());
                dispatchers.push(
                    Dispatcher::new(
                        control.clone(),
                        cache,
                        DispatcherConfig {
                            // Small batches so the pods interleave claims
                            // over the six rows instead of one pod taking
                            // everything in a single claim.
                            pipeline_batch_size: 2,
                            ..test_config(WorkerPoolsConfig::default())
                        },
                    )
                    .await
                    .expect("build dispatcher"),
                );
            }

            // Enqueue six durable pipeline rows through an inline-off core
            // (the dispatcher composition's save path: enqueue-only).
            let cache = dispatch_cache(&control);
            let core = cache
                .get_or_load(&tenant.account_id)
                .await
                .expect("load tenant")
                .manager
                .active_core()
                .await
                .expect("active core");
            let mut atom_ids = Vec::new();
            for i in 0..6 {
                let created = core
                    .create_atom(
                        atomic_core::CreateAtomRequest {
                            content: format!("note {i} about distributed claim semantics"),
                            ..Default::default()
                        },
                        |_| {},
                    )
                    .await
                    .expect("create atom")
                    .expect("atom inserted");
                atom_ids.push(created.atom.id.clone());
            }
            assert_eq!(
                core.count_pipeline_jobs().await.expect("count"),
                6,
                "saves must be enqueue-only under the dispatcher composition"
            );

            // Both pods tick concurrently until the ledger drains.
            let mut budget = 50;
            loop {
                let (o1, o2) = tokio::join!(dispatchers[0].tick(), dispatchers[1].tick());
                for handle in o1.handles.into_iter().chain(o2.handles) {
                    handle.await.expect("worker task");
                }
                if core.count_pipeline_jobs().await.expect("count") == 0 {
                    break;
                }
                budget -= 1;
                assert!(budget > 0, "pipeline ledger did not drain");
            }

            // Exactly-once: each atom completed embedding once across BOTH
            // pods' channels, and the durable state agrees.
            let mut completions: HashMap<String, usize> = HashMap::new();
            for rx in receivers.iter_mut() {
                while let Ok(event) = rx.try_recv() {
                    if let atomic_server::state::ServerEvent::EmbeddingComplete { atom_id } = event
                    {
                        *completions.entry(atom_id).or_insert(0) += 1;
                    }
                }
            }
            for atom_id in &atom_ids {
                assert_eq!(
                    completions.get(atom_id).copied().unwrap_or(0),
                    1,
                    "atom {atom_id} must be embedded exactly once across both pods"
                );
                let fetched = core
                    .get_atom(atom_id)
                    .await
                    .expect("get atom")
                    .expect("atom persisted");
                assert_eq!(fetched.atom.embedding_status, "complete");
            }
        },
    )
    .await;
}

/// Crash/restart reclaim: a worker that died mid-lease (claimed row,
/// dropped without settling) leaves a `running` row; once the lease
/// expires, a dispatcher claims it through the reclaim path and completes
/// it. The dispatcher never extends or bypasses the lease — expiry is
/// simulated by rewinding `lease_until` directly (a real 15-minute wait).
#[tokio::test]
async fn expired_lease_is_reclaimed_and_completed() {
    with_control_db(
        "expired_lease_is_reclaimed_and_completed",
        |url| async move {
            let control = connect_control(&url).await;
            let tenant = provision_tenant(&control, None, "alpha").await;

            // "Pod 1": claim draft_pipeline and crash mid-lease (dropping
            // the handle aborts the heartbeat and settles nothing).
            let manager = tenant_manager(&tenant).await;
            let core = manager.active_core().await.expect("active core");
            let handle = atomic_core::scheduler::ledger::claim_or_create(
                &core,
                "draft_pipeline",
                None,
                TaskRunTrigger::Schedule,
                3,
            )
            .await
            .expect("claim")
            .expect("claim won");
            drop(handle);

            let runs = core
                .list_task_runs("draft_pipeline", None, 10)
                .await
                .expect("list runs");
            assert_eq!(runs.len(), 1);
            assert!(
                matches!(runs[0].state, TaskRunState::Running),
                "crashed worker leaves the row running"
            );

            // Advance past the lease (in lieu of waiting out 15 minutes).
            let tenant_url = cluster_config()
                .tenant_db_url(&tenant.db_name)
                .expect("tenant url");
            let mut conn = PgConnection::connect(&tenant_url)
                .await
                .expect("connect tenant db");
            let rewound = sqlx::query(
                "UPDATE task_runs SET lease_until = '2000-01-01T00:00:00+00:00' \
                 WHERE task_id = 'draft_pipeline'",
            )
            .execute(&mut conn)
            .await
            .expect("rewind lease")
            .rows_affected();
            assert_eq!(rewound, 1);
            conn.close().await.expect("close");

            // "Pod 2": a fresh dispatcher reclaims and completes it.
            let cache = dispatch_cache(&control);
            let counting = CountingExecutor::new(Arc::new(CoreExecutor::new(Arc::clone(&cache))));
            let executor: Arc<dyn WorkExecutor> = counting.clone();
            let dispatcher = Dispatcher::with_executor(
                control.clone(),
                cache,
                test_config(WorkerPoolsConfig::default()),
                executor,
            )
            .await
            .expect("build dispatcher");

            let outcome = dispatcher.tick().await;
            for handle in outcome.handles {
                handle.await.expect("worker task");
            }

            assert!(
                counting
                    .executed()
                    .iter()
                    .any(|(acct, k)| acct == &tenant.account_id && k == "task:draft_pipeline"),
                "the second pod must execute the reclaimed run"
            );
            let runs = core
                .list_task_runs("draft_pipeline", None, 10)
                .await
                .expect("list runs");
            assert_eq!(runs.len(), 1, "reclaim reuses the row, never duplicates");
            assert!(
                matches!(runs[0].state, TaskRunState::Succeeded),
                "reclaimed row settled succeeded, got {:?}",
                runs[0].state
            );
        },
    )
    .await;
}

/// Hint lifecycle, empty side: a hinted tenant with no due work and empty
/// ledgers gets its hint cleared by the tick.
#[tokio::test]
async fn tick_clears_hint_for_idle_tenant() {
    with_control_db("tick_clears_hint_for_idle_tenant", |url| async move {
        let control = connect_control(&url).await;
        let tenant = provision_tenant(&control, None, "alpha").await;
        disable_system_tasks(&tenant).await;

        mark_hint(&control, &tenant.account_id).await.expect("mark");
        assert_eq!(list_hinted_accounts(&control).await.unwrap().len(), 1);

        let cache = dispatch_cache(&control);
        let dispatcher = Dispatcher::with_executor(
            control.clone(),
            Arc::clone(&cache),
            test_config(WorkerPoolsConfig::default()),
            RecordingExecutor::new(Duration::ZERO),
        )
        .await
        .expect("build dispatcher");

        let outcome = dispatcher.tick().await;
        for handle in outcome.handles {
            handle.await.expect("worker task");
        }

        assert_eq!(outcome.scheduled, 0, "idle tenant has nothing to run");
        assert_eq!(outcome.hints_cleared, 1);
        assert!(
            list_hinted_accounts(&control).await.unwrap().is_empty(),
            "the idle tenant's hint must be gone"
        );
    })
    .await;
}

/// Hint lifecycle, racing side: a hint (re)marked while the tick is in
/// flight survives the tick's clear — `clear_hint_if_older` only deletes
/// stamps at or before the one the scan read. Timing-dependent in the
/// harmless direction (an early bump is legitimately cleared and re-marked
/// at the loop top), so the assertion retries; an implementation that
/// cleared with a fresh stamp would fail every attempt.
#[tokio::test]
async fn hint_marked_mid_tick_survives() {
    with_control_db("hint_marked_mid_tick_survives", |url| async move {
        let control = connect_control(&url).await;
        let tenant = provision_tenant(&control, None, "alpha").await;
        disable_system_tasks(&tenant).await;

        let cache = dispatch_cache(&control);
        let dispatcher = Arc::new(
            Dispatcher::with_executor(
                control.clone(),
                Arc::clone(&cache),
                test_config(WorkerPoolsConfig::default()),
                RecordingExecutor::new(Duration::ZERO),
            )
            .await
            .expect("build dispatcher"),
        );

        let mut survived = false;
        for _ in 0..5 {
            mark_hint(&control, &tenant.account_id).await.expect("mark");
            let d = Arc::clone(&dispatcher);
            let tick = tokio::spawn(async move { d.tick().await });
            // Land a bump inside the tick's poll window (after the scan
            // read the hint, before it clears).
            tokio::time::sleep(Duration::from_millis(5)).await;
            mark_hint(&control, &tenant.account_id).await.expect("bump");
            let outcome = tick.await.expect("tick task");
            for handle in outcome.handles {
                handle.await.expect("worker task");
            }
            let hinted = list_hinted_accounts(&control).await.expect("list hints");
            if hinted.iter().any(|h| h.account_id == tenant.account_id) {
                survived = true;
                break;
            }
        }
        assert!(
            survived,
            "a hint bumped mid-tick must survive the tick's clear"
        );
    })
    .await;
}

/// The tenant pause gate: with `accounts.provider_paused_until` present
/// (added here exactly as the circuit-breaker phase will) and in the
/// future, the tenant is skipped wholesale — nothing polled, nothing
/// executed, hint untouched. Once the pause lapses, dispatch resumes.
#[tokio::test]
async fn paused_tenant_is_skipped_until_pause_lapses() {
    with_control_db(
        "paused_tenant_is_skipped_until_pause_lapses",
        |url| async move {
            let control = connect_control(&url).await;
            let tenant = provision_tenant(&control, None, "alpha").await;

            sqlx::query("ALTER TABLE accounts ADD COLUMN provider_paused_until TIMESTAMPTZ")
                .execute(control.pool())
                .await
                .expect("add pause column");
            sqlx::query(
                "UPDATE accounts SET provider_paused_until = NOW() + interval '1 hour' \
                 WHERE id = $1",
            )
            .bind(&tenant.account_id)
            .execute(control.pool())
            .await
            .expect("pause tenant");

            mark_hint(&control, &tenant.account_id).await.expect("mark");

            // Built AFTER the column exists, so detection finds it.
            let recorder = RecordingExecutor::new(Duration::ZERO);
            let dispatcher = Dispatcher::with_executor(
                control.clone(),
                dispatch_cache(&control),
                test_config(WorkerPoolsConfig::default()),
                recorder.clone(),
            )
            .await
            .expect("build dispatcher");

            let outcome = dispatcher.tick().await;
            for handle in outcome.handles {
                handle.await.expect("worker task");
            }
            assert_eq!(outcome.polled, 0, "paused tenant must not be polled");
            assert!(
                recorder.completions().is_empty(),
                "paused tenant must not execute work"
            );
            assert_eq!(
                list_hinted_accounts(&control).await.unwrap().len(),
                1,
                "the pause must not clear the hint — work waits, not vanishes"
            );

            // Pause lapses → the tenant dispatches again (a fresh tenant
            // always has draft_pipeline due).
            sqlx::query(
                "UPDATE accounts SET provider_paused_until = NOW() - interval '1 hour' \
                 WHERE id = $1",
            )
            .bind(&tenant.account_id)
            .execute(control.pool())
            .await
            .expect("unpause tenant");

            let outcome = dispatcher.tick().await;
            for handle in outcome.handles {
                handle.await.expect("worker task");
            }
            assert!(outcome.polled >= 1, "unpaused tenant polls again");
            assert!(
                recorder
                    .completions()
                    .iter()
                    .any(|(acct, _)| acct == &tenant.account_id),
                "unpaused tenant's due work must dispatch"
            );
        },
    )
    .await;
}

// ==================== Full e2e: HTTP → hint → pool → WS ====================

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The serve-shaped composition with the dispatcher ON: HTTP server wired
/// exactly like `configure_cloud_app`, cache with inline pipeline OFF, and
/// a running dispatcher loop owning all background execution.
struct DispatcherHarness {
    control: ControlPlane,
    mock: MockAiServer,
    client: reqwest::Client,
    port: u16,
    base_url: String,
    server: actix_web::dev::ServerHandle,
    dispatcher_loop: tokio::task::JoinHandle<()>,
    _fallback: FallbackAppState,
}

impl DispatcherHarness {
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

        // The dispatcher over the SAME cache the server resolves tenants
        // through — workers publish into the channels WS clients hold.
        let dispatcher = Arc::new(
            Dispatcher::new(
                control.clone(),
                Arc::clone(&cache),
                DispatcherConfig {
                    tick_interval: Duration::from_millis(100),
                    slow_scan_interval: Duration::from_secs(2),
                    ..DispatcherConfig::default()
                },
            )
            .await
            .expect("build dispatcher"),
        );
        let dispatcher_loop = tokio::spawn(dispatcher.run_loop());

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local addr").port();
        let state = fallback.data();
        let control_for_app = control.clone();
        let server = HttpServer::new(move || {
            App::new().configure(configure_cloud_app(
                state.clone(),
                auth.clone(),
                account_plane.clone(),
                tenant_plane.clone(),
                control_for_app.clone(),
            ))
        })
        .workers(1)
        .listen(listener)
        .expect("attach listener")
        .run();
        let handle = server.handle();
        actix_web::rt::spawn(server);

        DispatcherHarness {
            control,
            mock,
            client: reqwest::Client::new(),
            port,
            base_url: format!("http://127.0.0.1:{port}"),
            server: handle,
            dispatcher_loop,
            _fallback: fallback,
        }
    }

    async fn stop(self) {
        self.dispatcher_loop.abort();
        self.server.stop(false).await;
    }

    fn api(&self, method: Method, subdomain: &str, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base_url))
            .header(HOST, format!("{subdomain}.{BASE_DOMAIN}"))
    }

    async fn ws_connect(&self, subdomain: &str, token: &str) -> WsStream {
        let mut request = format!("ws://127.0.0.1:{}/ws", self.port)
            .into_client_request()
            .expect("ws request");
        let headers = request.headers_mut();
        headers.insert(
            "Host",
            format!("{subdomain}.{BASE_DOMAIN}").parse().expect("host"),
        );
        headers.insert(
            "Authorization",
            format!("Bearer {token}").parse().expect("auth header"),
        );
        let (ws, _resp) = tokio_tungstenite::connect_async(request)
            .await
            .expect("ws connect");
        ws
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

/// Full e2e through the pool: an atom created over HTTP (dispatcher ON, so
/// the save is enqueue-only) is executed by the dispatcher's embedding pool,
/// the pipeline reaches `complete`, and the tenant's WebSocket receives the
/// same event family the inline path emits.
#[actix_web::test]
async fn dispatcher_runs_pipeline_and_streams_ws_events() {
    with_control_db(
        "dispatcher_runs_pipeline_and_streams_ws_events",
        |url| async move {
            let h = DispatcherHarness::spawn(&url).await;
            let tenant = provision_tenant(&h.control, Some(&h.mock), "alpha").await;
            let token = issue_token(
                &h.control,
                &tenant.account_id,
                TokenScope::Account,
                None,
                "e2e",
            )
            .await
            .expect("issue token");

            let mut ws = h.ws_connect(&tenant.subdomain, &token).await;

            let resp = h
                .api(Method::POST, &tenant.subdomain, "/api/atoms")
                .bearer_auth(&token)
                .json(&json!({ "content": "Dispatcher-executed note about pour-over coffee." }))
                .send()
                .await
                .expect("send create atom");
            assert_eq!(resp.status(), StatusCode::CREATED);
            let atom: Value = resp.json().await.expect("atom json");
            let atom_id = atom["id"].as_str().expect("atom id").to_string();
            assert_eq!(
                atom["embedding_status"], "pending",
                "with the dispatcher on, the save must be enqueue-only"
            );

            // The pool's worker streams the identical event family the
            // inline path emits, onto the tenant's own channel.
            let frames = collect_until(&mut ws, EVENT_DEADLINE, |e| {
                e["type"] == "EmbeddingComplete" && e["atom_id"] == atom_id.as_str()
            })
            .await;
            assert!(
                frames.iter().any(|e| e["type"] == "PipelineQueueStarted"),
                "queue lifecycle events must stream to the tenant socket: {frames:?}"
            );

            // And the durable state agrees.
            let resp = h
                .api(
                    Method::GET,
                    &tenant.subdomain,
                    &format!("/api/atoms/{atom_id}"),
                )
                .bearer_auth(&token)
                .send()
                .await
                .expect("get atom");
            assert_eq!(resp.status(), StatusCode::OK);
            let body: Value = resp.json().await.expect("atom json");
            assert_eq!(body["embedding_status"], "complete");

            h.stop().await;
        },
    )
    .await;
}
