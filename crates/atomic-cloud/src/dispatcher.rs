//! The per-pod dispatcher (plan: "Worker fairness & job queue").
//!
//! Each `atomic-cloud serve` process runs one dispatcher: a tick loop that
//! discovers pending tenant-ledger work and feeds it to the bounded
//! [`WorkerPools`] with per-tenant round-robin fairness. There is no leader
//! election — the durable ledgers' conditional claims (`FOR UPDATE SKIP
//! LOCKED` on `atom_pipeline_jobs`, conditional UPDATEs + a partial unique
//! index on `task_runs`) are the cross-pod mutual exclusion, so N pods
//! ticking over the same tenants merely race claims they can't double-win.
//! Jittered tick offsets keep a fleet from synchronizing its polls.
//!
//! # One tick
//!
//! 1. **Scan** ([`Dispatcher::tick`]): read `dispatch_hints` (fast path —
//!    only tenants something has marked); on the slow-scan interval, add
//!    *every* active account (the bound on hint loss; see
//!    [`crate::dispatch_hints`]). Tenants whose `provider_paused_until` is
//!    in the future are skipped wholesale (the circuit-breaker phase writes
//!    that column; until it exists the gate reads as never-paused).
//! 2. **Poll** each candidate tenant (the plan's N+1 poll): resolve its
//!    [`TenantHandle`] through the [`AccountCache`], fan over its knowledge
//!    bases, and translate ledger state into [`WorkItem`]s — claimable
//!    pipeline batches, due system tasks, due feeds, runnable wiki-regen
//!    retries, due reports. Nothing is claimed here; items are *intents*,
//!    and the executor's claim decides who actually runs.
//! 3. **Hint lifecycle**: a tenant with no items and empty ledgers gets its
//!    hint cleared via `clear_hint_if_older` (a mid-scan enqueue survives by
//!    stamp comparison); a tenant discovered with work by the slow path gets
//!    its hint (re)marked so the fast path watches it from now on.
//! 4. **Drain** ([`Dispatcher::drain`]): round-robin over a deque of
//!    per-tenant deques — pop a tenant, admit ONE job into its class pool,
//!    push the tenant back. Tenants over their per-tenant cap (or whose
//!    admissible classes are all exhausted) park for the rest of the tick.
//!    Admitted jobs run on spawned workers that **claim-then-execute**
//!    through the existing atomic-core machinery; pool permits release on
//!    completion.
//!
//! Un-drained items are simply dropped at the end of the tick — they are
//! re-derived from the ledgers next tick. The same property is the restart
//! story: in-memory queues evaporate with the process, durable leases
//! expire, and the next scan (here or on a peer pod) reclaims the work. The
//! dispatcher never extends or bypasses lease semantics.
//!
//! # Events
//!
//! Workers route pipeline/ingestion events into the tenant's own event
//! channel (the [`AccountCache`] entry's `event_tx` — the channel the
//! tenant's WebSocket sessions subscribe to) through the same
//! `atomic-server::event_bridge` adapters the request path uses, so the
//! frontend experience is identical to inline execution.
//!
//! # Follow-on work
//!
//! Executed work often enqueues more ledger work (a feed poll creates atoms
//! whose pipeline jobs are now pending; a draft-pipeline pass enqueues
//! embedding jobs). Those writes don't pass through the data plane's
//! hint-marking middleware, so the worker re-marks the tenant's hint after
//! any execution that ran (or failed — failures leave backed-off retry rows
//! the fast path must keep watching).

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use atomic_core::models::TaskRunTrigger;
use atomic_core::scheduler::{runner, ScheduledTask, TaskContext};
use atomic_core::{ingest, reports, wiki, AtomicCore, TaskRun};
use atomic_server::event_bridge;
use atomic_server::state::ServerEvent;
use chrono::{DateTime, Utc};
use rand::Rng;
use tokio::task::JoinHandle;

use crate::account_cache::{AccountCache, TenantHandle};
use crate::control_plane::ControlPlane;
use crate::dispatch_hints::{
    clear_hint_if_older, list_active_account_ids, list_hinted_accounts, mark_hint,
};
use crate::error::CloudError;
use crate::pools::{WorkClass, WorkerPools, WorkerPoolsConfig};

/// Tuning knobs for the dispatcher. Every field is a `serve` CLI flag.
#[derive(Debug, Clone)]
pub struct DispatcherConfig {
    /// Pause between ticks. Each pod additionally offsets its first tick by
    /// a random fraction of this interval so a fleet booted together
    /// doesn't synchronize its control-plane polls.
    pub tick_interval: Duration,
    /// How often a tick also sweeps ALL active accounts instead of only
    /// hinted ones — the recovery bound for lost hint writes and for purely
    /// time-driven work (cron reports, feed intervals) on tenants nobody is
    /// mutating. The first tick after boot always full-scans.
    pub slow_scan_interval: Duration,
    /// Jobs per pipeline-batch claim. One batch occupies one embedding-pool
    /// slot for its whole execution, so this trades per-claim overhead
    /// against fairness granularity.
    pub pipeline_batch_size: i32,
    /// Per-tenant in-flight cap for report runs — a work-type override
    /// tighter than the llm class cap (plan table: reports per-tenant 1).
    pub reports_per_tenant_cap: usize,
    /// The four class pools' total / per-tenant caps.
    pub pools: WorkerPoolsConfig,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        Self {
            tick_interval: Duration::from_secs(2),
            slow_scan_interval: Duration::from_secs(300),
            pipeline_batch_size: 8,
            reports_per_tenant_cap: 1,
            pools: WorkerPoolsConfig::default(),
        }
    }
}

/// One schedulable unit of tenant work, scoped to a knowledge base
/// (`db_id`) inside the tenant's database. Items are *intents* derived from
/// a ledger scan — executing one starts with the ledger claim, so a stale
/// item (a peer already ran it; the report was deleted) executes as a
/// no-op `Skipped`.
#[derive(Debug, Clone)]
pub enum WorkItem {
    /// Claim up to `batch` due `atom_pipeline_jobs` and process them.
    PipelineBatch { db_id: String, batch: i32 },
    /// One due system task (`draft_pipeline`, `graph_maintenance`,
    /// `task_runs_gc`) through `scheduler::runner::run_task`.
    SystemTask { db_id: String, task_id: String },
    /// One due feed poll through `ingest::poller::run_feed_poll`.
    FeedPoll { db_id: String, feed_id: String },
    /// One runnable `wiki.regenerate` retry row, claimed via
    /// `wiki::runner::run_runnable_wiki_regen`. Carries the scanned row
    /// (boxed — it dwarfs the other variants); the conditional claim
    /// fences staleness.
    WikiRegen { db_id: String, run: Box<TaskRun> },
    /// One due report through `reports::run_report`.
    Report { db_id: String, report_id: String },
}

impl WorkItem {
    /// Which pool admits this item (plan table: "How each work-type lands").
    pub fn class(&self) -> WorkClass {
        match self {
            WorkItem::PipelineBatch { .. } => WorkClass::Embedding,
            WorkItem::SystemTask { .. } => WorkClass::Maintenance,
            WorkItem::FeedPoll { .. } => WorkClass::Ingestion,
            WorkItem::WikiRegen { .. } => WorkClass::Llm,
            WorkItem::Report { .. } => WorkClass::Llm,
        }
    }

    /// The knowledge base this item executes against.
    pub fn db_id(&self) -> &str {
        match self {
            WorkItem::PipelineBatch { db_id, .. }
            | WorkItem::SystemTask { db_id, .. }
            | WorkItem::FeedPoll { db_id, .. }
            | WorkItem::WikiRegen { db_id, .. }
            | WorkItem::Report { db_id, .. } => db_id,
        }
    }

    /// Work-type-specific per-tenant cap override (plan: reports = llm
    /// class with per-tenant cap 1).
    fn per_tenant_cap_override(&self, config: &DispatcherConfig) -> Option<usize> {
        match self {
            WorkItem::Report { .. } => Some(config.reports_per_tenant_cap),
            _ => None,
        }
    }
}

/// One tenant's pending items for a tick, in scan order. The drain loop's
/// round-robin operates over a deque of these.
#[derive(Debug)]
pub struct TenantQueue {
    pub account_id: String,
    pub items: VecDeque<WorkItem>,
}

/// What executing one item amounted to.
#[derive(Debug, Clone)]
pub enum ExecOutcome {
    /// Work ran to terminal completion (success or settled-empty).
    Executed,
    /// Nothing to do: the claim lost to a peer, the backoff window hasn't
    /// opened, or the subject vanished between scan and claim.
    Skipped,
    /// Work ran and failed; the durable ledger already took the
    /// retry-or-abandon decision.
    Failed(String),
}

/// Execution seam between the dispatcher's scheduling and the real
/// atomic-core machinery. Production uses [`CoreExecutor`]; tests inject
/// counting/recording executors to pin fairness and cap behavior without
/// real provider work.
#[async_trait::async_trait]
pub trait WorkExecutor: Send + Sync {
    async fn execute(&self, account_id: &str, item: &WorkItem) -> Result<ExecOutcome, CloudError>;
}

/// The production executor: resolves the tenant through the
/// [`AccountCache`], claims through the existing core machinery, and
/// bridges events into the tenant's channel.
pub struct CoreExecutor {
    cache: Arc<AccountCache>,
}

impl CoreExecutor {
    pub fn new(cache: Arc<AccountCache>) -> Self {
        Self { cache }
    }

    async fn resolve(
        &self,
        account_id: &str,
        db_id: &str,
    ) -> Result<(AtomicCore, TenantHandle), CloudError> {
        let handle = self.cache.get_or_load(account_id).await?;
        let core = handle
            .manager
            .get_core(db_id)
            .await
            .map_err(CloudError::core("resolving tenant core for dispatch"))?;
        Ok((core, handle))
    }
}

#[async_trait::async_trait]
impl WorkExecutor for CoreExecutor {
    async fn execute(&self, account_id: &str, item: &WorkItem) -> Result<ExecOutcome, CloudError> {
        let (core, handle) = self.resolve(account_id, item.db_id()).await?;
        let event_tx = handle.event_tx.clone();
        match item {
            WorkItem::PipelineBatch { batch, .. } => {
                let claimed = core
                    .run_pipeline_jobs_batch(
                        *batch,
                        event_bridge::embedding_event_callback(event_tx),
                    )
                    .await
                    .map_err(CloudError::core("running pipeline batch"))?;
                // Per-job failures settle on the jobs themselves (status
                // columns + queue events); the batch as a unit "executed"
                // iff the claim returned work.
                Ok(if claimed > 0 {
                    ExecOutcome::Executed
                } else {
                    ExecOutcome::Skipped
                })
            }

            WorkItem::SystemTask { db_id, task_id } => {
                let Some(task) = system_task(task_id) else {
                    return Err(CloudError::Invariant(format!(
                        "dispatcher scheduled unknown system task {task_id:?}"
                    )));
                };
                let ctx = TaskContext {
                    event_cb: event_bridge::task_event_callback(event_tx.clone()),
                    embedding_event_cb: Arc::new(event_bridge::embedding_event_callback(event_tx)),
                };
                match runner::run_task(&core, db_id, task.as_ref(), &ctx)
                    .await
                    .map_err(CloudError::core("dispatching system task"))?
                {
                    runner::DispatchOutcome::Succeeded => Ok(ExecOutcome::Executed),
                    runner::DispatchOutcome::Failed { error } => Ok(ExecOutcome::Failed(error)),
                    runner::DispatchOutcome::Skipped | runner::DispatchOutcome::NotDue => {
                        Ok(ExecOutcome::Skipped)
                    }
                }
            }

            WorkItem::FeedPoll { feed_id, .. } => {
                match ingest::poller::run_feed_poll(
                    &core,
                    feed_id,
                    TaskRunTrigger::Schedule,
                    event_bridge::ingestion_event_callback(event_tx.clone()),
                    event_bridge::embedding_event_callback(event_tx),
                )
                .await
                .map_err(CloudError::core("dispatching feed poll"))?
                {
                    ingest::poller::PollOutcome::Polled(_) => Ok(ExecOutcome::Executed),
                    ingest::poller::PollOutcome::Failed { error } => Ok(ExecOutcome::Failed(error)),
                    ingest::poller::PollOutcome::Skipped => Ok(ExecOutcome::Skipped),
                }
            }

            WorkItem::WikiRegen { run, .. } => {
                match wiki::runner::run_runnable_wiki_regen(&core, run)
                    .await
                    .map_err(CloudError::core("dispatching wiki regeneration"))?
                {
                    wiki::runner::RegenOutcome::Generated(_) => Ok(ExecOutcome::Executed),
                    wiki::runner::RegenOutcome::Failed { error } => Ok(ExecOutcome::Failed(error)),
                    wiki::runner::RegenOutcome::Skipped => Ok(ExecOutcome::Skipped),
                }
            }

            WorkItem::Report { report_id, .. } => {
                let Some(report) = core
                    .get_report(report_id)
                    .await
                    .map_err(CloudError::core("loading report for dispatch"))?
                else {
                    // Deleted between scan and execution — moot.
                    return Ok(ExecOutcome::Skipped);
                };
                match reports::run_report(&core, &report, TaskRunTrigger::Schedule)
                    .await
                    .map_err(CloudError::core("dispatching report run"))?
                {
                    reports::RunOutcome::Succeeded { finding_atom_id } => {
                        // The runner writes through storage without touching
                        // the event bridge; broadcast the finding so an open
                        // dashboard refreshes live (mirrors atomic-server's
                        // reports loop).
                        match core.get_atom(&finding_atom_id).await {
                            Ok(Some(atom)) => {
                                let _ = event_tx.send(ServerEvent::AtomCreated { atom });
                            }
                            Ok(None) => tracing::warn!(
                                report_id,
                                finding_atom_id,
                                "[dispatcher] finding atom missing after write — skipping broadcast"
                            ),
                            Err(e) => tracing::warn!(
                                report_id,
                                error = %e,
                                "[dispatcher] finding fetch for broadcast failed"
                            ),
                        }
                        Ok(ExecOutcome::Executed)
                    }
                    reports::RunOutcome::EmptyScope { .. } => Ok(ExecOutcome::Executed),
                    reports::RunOutcome::Failed { error } => Ok(ExecOutcome::Failed(error)),
                    reports::RunOutcome::Skipped => Ok(ExecOutcome::Skipped),
                }
            }
        }
    }
}

/// The system tasks the maintenance pool runs — the same registration set
/// as atomic-server's scheduler tick (`atomic-server/src/main.rs`).
fn system_tasks() -> Vec<Arc<dyn ScheduledTask>> {
    vec![
        Arc::new(atomic_core::pipeline_task::DraftPipelineTask),
        Arc::new(atomic_core::graph_maintenance::GraphMaintenanceTask),
        Arc::new(atomic_core::scheduler::gc::TaskRunsGcTask),
    ]
}

fn system_task(task_id: &str) -> Option<Arc<dyn ScheduledTask>> {
    system_tasks().into_iter().find(|t| t.id() == task_id)
}

/// What one tick did — counts for logs and tests, plus the spawned worker
/// handles so tests can await completion deterministically. The production
/// loop drops the handles (workers own pool permits; a slow LLM run must
/// not stall the next tick).
pub struct TickOutcome {
    /// Tenants polled this tick (hinted + slow-scan candidates, minus
    /// paused).
    pub polled: usize,
    /// Jobs admitted into pools this tick.
    pub scheduled: usize,
    /// Hints cleared because the tenant's ledgers were empty.
    pub hints_cleared: usize,
    /// Worker tasks spawned this tick.
    pub handles: Vec<JoinHandle<()>>,
}

/// Per-pod dispatcher. See the module docs for the tick anatomy. Cheap to
/// share via `Arc`; [`Dispatcher::run_loop`] is the serve binary's driver
/// and [`Dispatcher::tick`] / [`Dispatcher::drain`] are public so tests
/// drive scheduling deterministically.
pub struct Dispatcher {
    control: ControlPlane,
    cache: Arc<AccountCache>,
    pools: Arc<WorkerPools>,
    executor: Arc<dyn WorkExecutor>,
    config: DispatcherConfig,
    /// Whether `accounts.provider_paused_until` exists (it ships with the
    /// circuit-breaker phase; this phase gates on its presence so the pause
    /// check is live the moment the column lands). Detected once at build.
    pause_column_present: bool,
    last_slow_scan: Mutex<Option<Instant>>,
}

impl Dispatcher {
    /// Production construction: real [`CoreExecutor`] over the same cache
    /// the serving stack uses (so workers publish into the channels live
    /// WebSocket clients hold).
    pub async fn new(
        control: ControlPlane,
        cache: Arc<AccountCache>,
        config: DispatcherConfig,
    ) -> Result<Self, CloudError> {
        let executor = Arc::new(CoreExecutor::new(Arc::clone(&cache)));
        Self::with_executor(control, cache, config, executor).await
    }

    /// Test seam: same dispatcher, custom executor.
    pub async fn with_executor(
        control: ControlPlane,
        cache: Arc<AccountCache>,
        config: DispatcherConfig,
        executor: Arc<dyn WorkExecutor>,
    ) -> Result<Self, CloudError> {
        let pause_column_present = pause_column_exists(&control).await?;
        Ok(Self {
            control,
            cache,
            pools: Arc::new(WorkerPools::new(config.pools)),
            executor,
            config,
            pause_column_present,
            last_slow_scan: Mutex::new(None),
        })
    }

    /// The pools, for instrumentation in tests and metrics.
    pub fn pools(&self) -> &Arc<WorkerPools> {
        &self.pools
    }

    /// Run ticks forever, offset by a random fraction of the tick interval
    /// so pods booted together don't synchronize. Never returns; the serve
    /// binary `select!`s it against the HTTP server.
    pub async fn run_loop(self: Arc<Self>) {
        let jitter = Duration::from_millis(
            rand::thread_rng().gen_range(0..=self.config.tick_interval.as_millis().max(1) as u64),
        );
        tokio::time::sleep(jitter).await;
        let mut ticker = tokio::time::interval(self.config.tick_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let outcome = self.tick().await;
            if outcome.scheduled > 0 {
                tracing::debug!(
                    polled = outcome.polled,
                    scheduled = outcome.scheduled,
                    "[dispatcher] tick"
                );
            }
            // Workers own their permits; the tick never waits on them.
            drop(outcome.handles);
        }
    }

    /// One full tick: scan, poll, hint lifecycle, drain. Errors inside are
    /// logged and skipped per tenant — a broken tenant (or a control-plane
    /// hiccup) must not stall everyone else.
    pub async fn tick(&self) -> TickOutcome {
        let mut outcome = TickOutcome {
            polled: 0,
            scheduled: 0,
            hints_cleared: 0,
            handles: Vec::new(),
        };

        let candidates = match self.scan_candidates().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "[dispatcher] candidate scan failed; skipping tick");
                return outcome;
            }
        };
        let paused = self.paused_accounts(&candidates).await;

        let mut queues: VecDeque<TenantQueue> = VecDeque::new();
        for (account_id, hint_stamp) in candidates {
            if paused.contains(&account_id) {
                // Dispatch is paused, not the work: the hint (and the
                // ledger rows behind it) stay put for when the pause lifts.
                continue;
            }
            outcome.polled += 1;
            let poll = match self.poll_tenant(&account_id).await {
                Ok(p) => p,
                Err(e) => {
                    // Leave the hint alone — the next tick (or the slow
                    // scan) retries this tenant.
                    tracing::warn!(
                        account_id,
                        error = %e,
                        "[dispatcher] tenant poll failed; skipping this tick"
                    );
                    continue;
                }
            };

            let has_work = !poll.items.is_empty() || poll.ledger_active;
            match (has_work, hint_stamp) {
                (false, Some(stamp)) => {
                    // Ledgers empty: clear, unless an enqueue bumped the
                    // hint after our scan read it (the dual-write bound).
                    match clear_hint_if_older(&self.control, &account_id, stamp).await {
                        Ok(true) => outcome.hints_cleared += 1,
                        Ok(false) => {}
                        Err(e) => {
                            tracing::warn!(account_id, error = %e, "[dispatcher] hint clear failed")
                        }
                    }
                }
                (true, None) => {
                    // Slow-path discovery: re-arm the fast path so this
                    // tenant is watched every tick until it drains.
                    if let Err(e) = mark_hint(&self.control, &account_id).await {
                        tracing::warn!(account_id, error = %e, "[dispatcher] hint re-mark failed");
                    }
                }
                _ => {}
            }

            if !poll.items.is_empty() {
                queues.push_back(TenantQueue {
                    account_id,
                    items: poll.items,
                });
            }
        }

        let (scheduled, handles) = self.drain(&mut queues).await;
        outcome.scheduled = scheduled;
        outcome.handles = handles;
        outcome
    }

    /// Round-robin admission over per-tenant queues: pop a tenant, admit
    /// its first item whose class pool accepts it, push the tenant back.
    /// A tenant with no admissible item parks for the rest of this drain
    /// (its items stay in `queues` for callers that re-drain; the tick
    /// loop drops them and re-derives next tick). Returns the number of
    /// jobs admitted and their worker handles.
    pub async fn drain(&self, queues: &mut VecDeque<TenantQueue>) -> (usize, Vec<JoinHandle<()>>) {
        let mut handles = Vec::new();
        let mut parked: VecDeque<TenantQueue> = VecDeque::new();
        let mut scheduled = 0usize;

        while let Some(mut tq) = queues.pop_front() {
            if tq.items.is_empty() {
                continue;
            }
            // First admissible item, not strictly the head: a saturated
            // class must not head-of-line-block the tenant's other classes.
            let admitted = (0..tq.items.len()).find_map(|idx| {
                let item = &tq.items[idx];
                self.pools
                    .try_acquire(
                        item.class(),
                        &tq.account_id,
                        item.per_tenant_cap_override(&self.config),
                    )
                    .map(|permit| (idx, permit))
            });
            match admitted {
                Some((idx, permit)) => {
                    let item = tq.items.remove(idx).expect("index in bounds");
                    handles.push(self.spawn_worker(tq.account_id.clone(), item, permit));
                    scheduled += 1;
                    if !tq.items.is_empty() {
                        queues.push_back(tq);
                    }
                }
                None => parked.push_back(tq),
            }
        }

        *queues = parked;
        (scheduled, handles)
    }

    fn spawn_worker(
        &self,
        account_id: String,
        item: WorkItem,
        permit: crate::pools::PoolPermit,
    ) -> JoinHandle<()> {
        let executor = Arc::clone(&self.executor);
        let control = self.control.clone();
        tokio::spawn(async move {
            // Held for the full execution; releases the class + tenant
            // slots on drop (including panic/cancellation).
            let _permit = permit;
            let outcome = executor.execute(&account_id, &item).await;
            let remark_hint = match &outcome {
                // Executed work may have enqueued follow-on ledger work;
                // failures leave backed-off retry rows. Both need the fast
                // path watching this tenant (module docs: follow-on work).
                Ok(ExecOutcome::Executed) | Ok(ExecOutcome::Failed(_)) => true,
                Ok(ExecOutcome::Skipped) => false,
                Err(_) => false,
            };
            match outcome {
                Ok(ExecOutcome::Executed) | Ok(ExecOutcome::Skipped) => {}
                Ok(ExecOutcome::Failed(error)) => {
                    tracing::warn!(
                        account_id,
                        item = ?item,
                        error = %error,
                        "[dispatcher] work failed; ledger scheduled retry or abandoned"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        account_id,
                        item = ?item,
                        error = %e,
                        "[dispatcher] work dispatch errored"
                    );
                }
            }
            if remark_hint {
                if let Err(e) = mark_hint(&control, &account_id).await {
                    tracing::warn!(account_id, error = %e, "[dispatcher] follow-on hint mark failed");
                }
            }
        })
    }

    /// The tick's tenant candidates: every hinted account (with the stamp
    /// `clear_hint_if_older` fences on), plus — when the slow scan is due —
    /// every active account without a hint.
    async fn scan_candidates(&self) -> Result<Vec<(String, Option<DateTime<Utc>>)>, CloudError> {
        let hinted = list_hinted_accounts(&self.control).await?;
        let mut seen: HashSet<String> = hinted.iter().map(|h| h.account_id.clone()).collect();
        let mut candidates: Vec<(String, Option<DateTime<Utc>>)> = hinted
            .into_iter()
            .map(|h| (h.account_id, Some(h.last_enqueued_at)))
            .collect();

        if self.take_slow_scan_due() {
            for account_id in list_active_account_ids(&self.control).await? {
                if seen.insert(account_id.clone()) {
                    candidates.push((account_id, None));
                }
            }
        }
        Ok(candidates)
    }

    /// Whether this tick should full-scan, advancing the marker when it
    /// does. The first tick after boot always full-scans — restart recovery
    /// should not wait out a whole interval.
    fn take_slow_scan_due(&self) -> bool {
        let mut last = self
            .last_slow_scan
            .lock()
            .expect("slow-scan marker poisoned");
        let due = last
            .map(|t| t.elapsed() >= self.config.slow_scan_interval)
            .unwrap_or(true);
        if due {
            *last = Some(Instant::now());
        }
        due
    }

    /// The tenant-pause gate (plan: per-tenant circuit breaker). The
    /// breaker phase writes `accounts.provider_paused_until`; until the
    /// column exists this returns the empty set. Failures fail open with a
    /// warning — an unreadable pause column must not stop all dispatch.
    async fn paused_accounts(
        &self,
        candidates: &[(String, Option<DateTime<Utc>>)],
    ) -> HashSet<String> {
        if !self.pause_column_present || candidates.is_empty() {
            return HashSet::new();
        }
        let ids: Vec<String> = candidates.iter().map(|(id, _)| id.clone()).collect();
        let result: Result<Vec<String>, sqlx::Error> = sqlx::query_scalar(
            "SELECT id FROM accounts \
             WHERE id = ANY($1) \
               AND provider_paused_until IS NOT NULL \
               AND provider_paused_until > NOW()",
        )
        .bind(&ids)
        .fetch_all(self.control.pool())
        .await;
        match result {
            Ok(paused) => paused.into_iter().collect(),
            Err(e) => {
                tracing::warn!(error = %e, "[dispatcher] pause lookup failed; assuming none paused");
                HashSet::new()
            }
        }
    }

    /// The N+1 poll: translate one tenant's ledger state into work items.
    /// Read-only — claims happen in the workers.
    async fn poll_tenant(&self, account_id: &str) -> Result<TenantPoll, CloudError> {
        let handle = self.cache.get_or_load(account_id).await?;
        let (databases, _) = handle
            .manager
            .list_databases()
            .await
            .map_err(CloudError::core("listing tenant knowledge bases"))?;

        let mut items = VecDeque::new();
        let mut ledger_active = false;
        for db in &databases {
            let core = handle
                .manager
                .get_core(&db.id)
                .await
                .map_err(CloudError::core("resolving tenant core for poll"))?;
            let scan = || CloudError::core("scanning tenant ledgers");

            // Embedding: enough batch items to use the tenant's full
            // per-tenant allowance when the backlog warrants it (capacity
            // permitting); the backlog beyond that re-derives next tick.
            let batch = self.config.pipeline_batch_size.max(1);
            let due_jobs = core.count_due_pipeline_jobs().await.map_err(scan())?;
            if due_jobs > 0 {
                let batches = (due_jobs as usize).div_ceil(batch as usize);
                let max_items = self.config.pools.embedding.per_tenant.max(1);
                for _ in 0..batches.min(max_items) {
                    items.push_back(WorkItem::PipelineBatch {
                        db_id: db.id.clone(),
                        batch,
                    });
                }
            }

            // Maintenance: due system tasks (the cheap settings-table
            // is_due gate; run_task re-checks before claiming).
            for task in system_tasks() {
                if task.is_due(&core).await {
                    items.push_back(WorkItem::SystemTask {
                        db_id: db.id.clone(),
                        task_id: task.id().to_string(),
                    });
                }
            }

            // Ingestion: one item per due feed.
            for feed in core.list_due_feeds().await.map_err(scan())? {
                items.push_back(WorkItem::FeedPoll {
                    db_id: db.id.clone(),
                    feed_id: feed.id,
                });
            }

            // LLM: runnable wiki-regen retries (event-triggered — nothing
            // re-fires them but a ledger scan)…
            for run in core
                .list_runnable_task_runs(wiki::runner::WIKI_REGENERATE_TASK_ID)
                .await
                .map_err(scan())?
            {
                items.push_back(WorkItem::WikiRegen {
                    db_id: db.id.clone(),
                    run: Box::new(run),
                });
            }

            // …and due reports.
            let now = Utc::now();
            for report in core.list_enabled_reports().await.map_err(scan())? {
                if reports::schedule::is_due(&report, now) {
                    items.push_back(WorkItem::Report {
                        db_id: db.id.clone(),
                        report_id: report.id,
                    });
                }
            }

            // Hint lifecycle input: ANY non-terminal ledger row — in-flight
            // leases and backed-off retries included — keeps the hint (and
            // therefore the fast-path poll) alive. Cleared hints rely on
            // the slow scan, which is too coarse for a backoff window.
            if !ledger_active {
                ledger_active = core.count_pipeline_jobs().await.map_err(scan())? > 0;
            }
            if !ledger_active {
                ledger_active = core.count_active_task_runs().await.map_err(scan())? > 0;
            }
        }

        Ok(TenantPoll {
            items,
            ledger_active,
        })
    }
}

/// One tenant's poll result.
struct TenantPoll {
    items: VecDeque<WorkItem>,
    /// Whether any non-terminal ledger row exists at all (even ones that
    /// produced no item this tick, e.g. a backed-off retry) — the "keep
    /// the hint" signal.
    ledger_active: bool,
}

/// Whether `accounts.provider_paused_until` exists yet (see
/// [`Dispatcher::paused_accounts`]).
async fn pause_column_exists(control: &ControlPlane) -> Result<bool, CloudError> {
    sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM information_schema.columns
            WHERE table_schema = 'public'
              AND table_name = 'accounts'
              AND column_name = 'provider_paused_until'
         )",
    )
    .fetch_one(control.pool())
    .await
    .map_err(CloudError::db("checking for provider_paused_until column"))
}
