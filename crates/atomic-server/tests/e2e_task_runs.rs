//! End-to-end tests for the durable task-runs scheduler dispatch.
//!
//! Phase 2 of the durable-task-runs plan routes system tasks through the
//! `task_runs` ledger: each due `(task, database)` pair becomes a claimed
//! row with a durable lease, retry/backoff on failure, and a `last_run`
//! fast-path advance on terminal success. The production 15s loop in
//! `main.rs` is a thin timer around `scheduler::runner::tick_all_databases`,
//! so this suite drives ticks directly (no timers, no sleeps) and asserts
//! the per-backend storage contract through `AtomicCore::list_task_runs`.
//!
//! Stub `ScheduledTask` impls stand in for the real system tasks — the
//! point here is the dispatch semantics (backoff gating, lease blocking,
//! per-DB independence), not draft-pipeline or graph-maintenance behavior,
//! which have their own coverage in atomic-core.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use actix_web::test as actix_test;
use async_trait::async_trait;
use atomic_core::scheduler::runner::tick_all_databases;
use atomic_core::scheduler::{
    ledger, state as sched_state, ScheduledTask, TaskContext, TaskError, TaskRegistry,
};
use atomic_core::{AtomicCore, TaskRunState, TaskRunTrigger};
use chrono::Utc;
use serde_json::{json, Value};
use support::{test_app, Backend, TestCtx};

// ==================== Stub tasks ====================

/// How the stub decides each run's outcome. `FailIfAtomsPresent` keys
/// failure on per-DB data so the multi-DB test can make one database fail
/// while another succeeds with a single registered task instance.
enum StubMode {
    Succeed,
    Fail,
    FailIfAtomsPresent,
}

/// Minimal `ScheduledTask` with a shared invocation counter. Due-ness is
/// the trait default (enabled + interval elapsed since last success), so a
/// never-succeeded task is always due — exactly the state the backoff
/// assertions need.
///
/// `always_due` bypasses the settings-based gate entirely. The multi-DB
/// test needs it on Postgres, where the `settings` table is global across
/// logical databases (no `db_id` scoping — a pre-existing backend gap), so
/// one database's `last_run` write would race the other database's
/// `is_due` check within the same tick. With the gate constant, the only
/// thing throttling re-runs is the ledger itself — which is precisely the
/// per-DB machinery this suite is pinning down.
struct StubTask {
    id: &'static str,
    mode: StubMode,
    always_due: bool,
    runs: Arc<AtomicUsize>,
}

impl StubTask {
    fn new(id: &'static str, mode: StubMode) -> Self {
        Self {
            id,
            mode,
            always_due: false,
            runs: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn always_due(id: &'static str, mode: StubMode) -> Self {
        Self {
            always_due: true,
            ..Self::new(id, mode)
        }
    }
}

#[async_trait]
impl ScheduledTask for StubTask {
    fn id(&self) -> &'static str {
        self.id
    }

    fn display_name(&self) -> &'static str {
        "E2E stub task"
    }

    fn default_interval(&self) -> Duration {
        Duration::from_secs(60)
    }

    async fn is_due(&self, core: &AtomicCore) -> bool {
        if self.always_due {
            return true;
        }
        // Mirror the trait default; reimplemented because an override
        // can't delegate back to the default method.
        sched_state::is_due(core, self.id, self.default_interval(), true).await
    }

    async fn run(&self, core: &AtomicCore, _ctx: &TaskContext) -> Result<(), TaskError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        match self.mode {
            StubMode::Succeed => Ok(()),
            StubMode::Fail => Err(TaskError::Other("e2e stub failure".to_string())),
            StubMode::FailIfAtomsPresent => {
                let count = core
                    .count_atoms()
                    .await
                    .map_err(|e| TaskError::Other(e.to_string()))?;
                if count > 0 {
                    Err(TaskError::Other(format!("{count} marker atoms present")))
                } else {
                    Ok(())
                }
            }
        }
    }
}

// ==================== Helpers ====================

fn noop_ctx() -> TaskContext {
    TaskContext {
        event_cb: Arc::new(|_| {}),
        embedding_event_cb: Arc::new(|_| {}),
    }
}

fn registry_with(task: StubTask) -> Arc<TaskRegistry> {
    let mut registry = TaskRegistry::new();
    registry.register(Arc::new(task));
    Arc::new(registry)
}

/// Drive one scheduler tick over every database and wait for all spawned
/// dispatches to finish — the deterministic stand-in for the 15s timer.
async fn tick_and_join(ctx: &TestCtx, registry: &Arc<TaskRegistry>, task_ctx: &TaskContext) {
    let handles = tick_all_databases(&ctx.state.manager, registry, task_ctx).await;
    for handle in handles {
        handle.await.expect("dispatch task panicked");
    }
}

async fn active_core(ctx: &TestCtx) -> AtomicCore {
    ctx.state.manager.active_core().await.expect("active core")
}

// ==================== T1. Success records a run + advances last_run ====================

#[actix_web::test]
async fn tick_records_success_and_advances_last_run_sqlite() {
    run_tick_records_success_and_advances_last_run(Backend::Sqlite).await;
}

#[actix_web::test]
async fn tick_records_success_and_advances_last_run_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "tick_records_success_and_advances_last_run_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_tick_records_success_and_advances_last_run(Backend::Postgres).await;
}

async fn run_tick_records_success_and_advances_last_run(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let task = StubTask::new("e2e_success", StubMode::Succeed);
    let runs = Arc::clone(&task.runs);
    let registry = registry_with(task);
    let task_ctx = noop_ctx();

    tick_and_join(&ctx, &registry, &task_ctx).await;

    let core = active_core(&ctx).await;
    let history = core.list_task_runs("e2e_success", None, 10).await.unwrap();
    assert_eq!(history.len(), 1, "one ledger row per firing");
    let run = &history[0];
    assert_eq!(run.state, TaskRunState::Succeeded);
    assert_eq!(run.trigger, TaskRunTrigger::Schedule);
    assert_eq!(run.attempts, 1);
    assert!(run.lease_until.is_none(), "terminal rows clear the lease");
    assert!(run.finished_at.is_some());
    assert!(
        sched_state::get_last_run(&core, "e2e_success")
            .await
            .unwrap()
            .is_some(),
        "success advances the last_run fast-path"
    );

    // A second tick inside the interval is NotDue: no run, no new row.
    tick_and_join(&ctx, &registry, &task_ctx).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1, "not-due tick must not run");
    let history = core.list_task_runs("e2e_success", None, 10).await.unwrap();
    assert_eq!(history.len(), 1, "not-due tick must not touch the ledger");
}

// ==================== T2. Failure backs off instead of retrying every tick ====================

#[actix_web::test]
async fn failing_task_backs_off_and_keeps_last_run_unset_sqlite() {
    run_failing_task_backs_off_and_keeps_last_run_unset(Backend::Sqlite).await;
}

#[actix_web::test]
async fn failing_task_backs_off_and_keeps_last_run_unset_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "failing_task_backs_off_and_keeps_last_run_unset_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_failing_task_backs_off_and_keeps_last_run_unset(Backend::Postgres).await;
}

async fn run_failing_task_backs_off_and_keeps_last_run_unset(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let task = StubTask::new("e2e_failing", StubMode::Fail);
    let runs = Arc::clone(&task.runs);
    let registry = registry_with(task);
    let task_ctx = noop_ctx();

    tick_and_join(&ctx, &registry, &task_ctx).await;

    let core = active_core(&ctx).await;
    let history = core.list_task_runs("e2e_failing", None, 10).await.unwrap();
    assert_eq!(history.len(), 1);
    let run = &history[0];
    assert_eq!(run.state, TaskRunState::Pending, "retryable, not terminal");
    assert_eq!(run.attempts, 1);
    assert_eq!(run.last_error.as_deref(), Some("e2e stub failure"));
    assert!(
        run.next_attempt_at.as_str() > Utc::now().to_rfc3339().as_str(),
        "failure pushed next_attempt_at into the future (backoff)"
    );
    assert!(
        sched_state::get_last_run(&core, "e2e_failing")
            .await
            .unwrap()
            .is_none(),
        "failure must not advance last_run"
    );

    // The retry-storm regression: the task is still due (last_run never
    // advanced), but the backoff window gates the claim. Several more
    // ticks must not produce a second attempt or a second row.
    for _ in 0..3 {
        tick_and_join(&ctx, &registry, &task_ctx).await;
    }
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "no re-attempt inside the backoff window"
    );
    let history = core.list_task_runs("e2e_failing", None, 10).await.unwrap();
    assert_eq!(history.len(), 1, "backed-off row is reused, not duplicated");
    assert_eq!(history[0].attempts, 1);
}

// ==================== T3. Durable lease blocks dispatch, not the in-memory lock ====================

#[actix_web::test]
async fn live_lease_blocks_dispatch_until_released_sqlite() {
    run_live_lease_blocks_dispatch_until_released(Backend::Sqlite).await;
}

#[actix_web::test]
async fn live_lease_blocks_dispatch_until_released_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "live_lease_blocks_dispatch_until_released_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_live_lease_blocks_dispatch_until_released(Backend::Postgres).await;
}

async fn run_live_lease_blocks_dispatch_until_released(backend: Backend) {
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let core = active_core(&ctx).await;

    // A "peer" (another process in real life) claims the task's row. Our
    // registry's in-memory lock knows nothing about it — the durable lease
    // alone must block the dispatch.
    let peer = ledger::claim_or_create(&core, "e2e_leased", None, TaskRunTrigger::Schedule, 3)
        .await
        .unwrap()
        .expect("peer claims the run");

    let task = StubTask::new("e2e_leased", StubMode::Succeed);
    let runs = Arc::clone(&task.runs);
    let registry = registry_with(task);
    let task_ctx = noop_ctx();

    tick_and_join(&ctx, &registry, &task_ctx).await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "live lease must block dispatch even with a fresh in-memory lock"
    );

    // Peer finishes; the task is still due (our process never succeeded),
    // so the next tick opens a fresh row and runs.
    assert!(peer.complete(None).await.unwrap());
    tick_and_join(&ctx, &registry, &task_ctx).await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "dispatch resumes after release"
    );

    let history = core.list_task_runs("e2e_leased", None, 10).await.unwrap();
    assert_eq!(history.len(), 2, "peer's row + our row");
    assert!(history.iter().all(|r| r.state == TaskRunState::Succeeded));
}

// ==================== T4. Two databases make progress independently ====================

#[actix_web::test]
async fn two_databases_progress_independently_sqlite() {
    run_two_databases_progress_independently(Backend::Sqlite).await;
}

#[actix_web::test]
async fn two_databases_progress_independently_postgres() {
    if std::env::var("ATOMIC_TEST_DATABASE_URL").is_err() {
        eprintln!(
            "two_databases_progress_independently_postgres: skipping (ATOMIC_TEST_DATABASE_URL not set)"
        );
        return;
    }
    run_two_databases_progress_independently(Backend::Postgres).await;
}

async fn run_two_databases_progress_independently(backend: Backend) {
    let settings_isolated = matches!(backend, Backend::Sqlite);
    let Some(ctx) = TestCtx::new(backend).await else {
        return;
    };
    let app = actix_test::init_service(test_app(&ctx)).await;

    // Second database via the REST surface (same as a real client).
    let req = actix_test::TestRequest::post()
        .uri("/api/databases")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "name": "beta" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201, "create second database");
    let body: Value = actix_test::read_body_json(resp).await;
    let beta_id = body["id"].as_str().expect("database id").to_string();

    // Marker atom in the *default* database makes the stub fail there;
    // beta stays empty and succeeds. One registered task, two databases,
    // divergent outcomes.
    let req = actix_test::TestRequest::post()
        .uri("/api/atoms")
        .insert_header(ctx.auth_header())
        .set_json(json!({ "content": "marker atom — make the stub fail here" }))
        .to_request();
    let resp = actix_test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201, "seed marker atom in default db");

    // `always_due` keeps the test deterministic on Postgres (see the
    // StubTask docs): progress is observed purely through the ledger,
    // which is per-database on both backends.
    let task = StubTask::always_due("e2e_multi_db", StubMode::FailIfAtomsPresent);
    let runs = Arc::clone(&task.runs);
    let registry = registry_with(task);
    let task_ctx = noop_ctx();

    tick_and_join(&ctx, &registry, &task_ctx).await;
    assert_eq!(runs.load(Ordering::SeqCst), 2, "one run per database");

    let default_core = active_core(&ctx).await;
    let beta_core = ctx
        .state
        .manager
        .get_core(&beta_id)
        .await
        .expect("beta core");

    // Default DB: failed attempt, backoff pending.
    let default_history = default_core
        .list_task_runs("e2e_multi_db", None, 10)
        .await
        .unwrap();
    assert_eq!(default_history.len(), 1);
    assert_eq!(default_history[0].state, TaskRunState::Pending);
    assert_eq!(default_history[0].attempts, 1);
    assert!(default_history[0].last_error.is_some());

    // Beta DB: clean success with its own independent ledger row — the
    // failing sibling didn't hold it back.
    let beta_history = beta_core
        .list_task_runs("e2e_multi_db", None, 10)
        .await
        .unwrap();
    assert_eq!(beta_history.len(), 1);
    assert_eq!(beta_history[0].state, TaskRunState::Succeeded);
    assert!(sched_state::get_last_run(&beta_core, "e2e_multi_db")
        .await
        .unwrap()
        .is_some());

    // last_run isolation is a per-DB settings property. SQLite gets it
    // structurally (one settings table per database file); the Postgres
    // backend's settings table is currently global across logical
    // databases, so the assertion only holds on SQLite. The ledger
    // assertions above are the cross-backend contract.
    if settings_isolated {
        assert!(sched_state::get_last_run(&default_core, "e2e_multi_db")
            .await
            .unwrap()
            .is_none());
    }

    // Next tick: default is inside its backoff window (Skipped) while
    // beta — always due, terminal row settled — opens a fresh run. The
    // throttled database doesn't gate its sibling's progress.
    tick_and_join(&ctx, &registry, &task_ctx).await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        3,
        "beta progresses while default sits out its backoff"
    );
    let default_history = default_core
        .list_task_runs("e2e_multi_db", None, 10)
        .await
        .unwrap();
    assert_eq!(
        default_history.len(),
        1,
        "default reuses its backed-off row"
    );
    assert_eq!(default_history[0].attempts, 1, "no retry inside backoff");
    let beta_history = beta_core
        .list_task_runs("e2e_multi_db", None, 10)
        .await
        .unwrap();
    assert_eq!(beta_history.len(), 2, "beta recorded a second run");
}
