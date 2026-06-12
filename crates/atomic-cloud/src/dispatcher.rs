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
//!    in the future are skipped wholesale — the provider circuit breaker
//!    ([`crate::backpressure`]) writes that column; their ledger work sits
//!    durably until the pause lapses or a provider mutation clears it.
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
//! # Events — single-pod fidelity only
//!
//! Workers route pipeline/ingestion events into the tenant's own event
//! channel (the [`AccountCache`] entry's `event_tx` — the channel the
//! tenant's WebSocket sessions subscribe to) through the same
//! `atomic-server::event_bridge` adapters the request path uses.
//!
//! **This delivery is per-pod, in-memory.** The channel a worker publishes
//! into is the executing pod's cache entry; a WebSocket session subscribed
//! on a *different* pod — or on this pod after the entry was evicted and
//! rebuilt mid-execution — receives none of that execution's progress
//! events. Durable state is always correct (ledger rows, atom statuses,
//! and artifacts land in the tenant database regardless), and the frontend
//! self-heals on its next fetch, but live progress is only faithful when
//! the executing pod and the subscribed pod are the same — i.e.
//! single-pod deployments. A cross-pod event relay (e.g. Postgres
//! LISTEN/NOTIFY fan-out) is a planned follow-up for multi-pod
//! deployments; until then, expect missing WS progress events whenever a
//! peer pod wins the claim.
//!
//! # Follow-on work
//!
//! Executed work often enqueues more ledger work (a feed poll creates atoms
//! whose pipeline jobs are now pending; a draft-pipeline pass enqueues
//! embedding jobs). Those writes don't pass through the data plane's
//! hint-marking middleware, so the worker re-marks the tenant's hint after
//! any execution that ran (or failed — failures leave backed-off retry rows
//! the fast path must keep watching).
//!
//! # What is deliberately NOT here: streaming chat
//!
//! Every ledger-backed work-type flows through these pools; streaming chat
//! does not (plan: "Streaming chat (not in a pool)"). It is request-driven,
//! user-facing, and latency-critical — queueing a chat send behind another
//! tenant's wiki synthesis would be product-breaking, and there is no
//! durable row to re-claim on restart anyway. Its bound is a per-account
//! semaphore at the route instead: [`crate::chat_streams`], wired into the
//! data plane by `configure_cloud_app`.
//!
//! # Provider backpressure (plan: "Provider rate-limit handling")
//!
//! [`CoreExecutor`] classifies provider failures
//! ([`classify_provider_failure`]) out of both ledgers' failure surfaces
//! and applies the two layers:
//!
//! - **Layer 1, local backoff — jobs sit, they never fail.** For the
//!   `task_runs` ledger, the tenant cores run with the
//!   [`provider_failure_policy`](crate::backpressure::provider_failure_policy)
//!   installed (see [`crate::account_cache`]): a provider-classified
//!   failure *defers* the row (`RunHandle::defer_until` — lease released,
//!   `next_attempt_at` set to the provider's `Retry-After` clamped to
//!   [`DispatcherConfig::retry_after_cap`], or the pause-recheck horizon
//!   for credits/auth) **without consuming retry budget**, so a month of
//!   credit exhaustion can't burn `max_attempts` and terminally abandon
//!   wiki regeneration. Logic failures keep the ledger's own exponential
//!   backoff and attempt accounting. Pipeline-job failures are terminal in
//!   core (atom status `failed`, row cleared), so the executor
//!   **re-enqueues** provider-classified atoms with `not_before` honoring
//!   the same horizons (default [`RATE_LIMIT_REQUEUE_DELAY`]) — the job
//!   *sits in the ledger* and the claim predicate keeps it undispatchable
//!   until `not_before` passes. A provider mutation (rotation, activation,
//!   model change) re-arms both ledgers' provider-held horizons to "now"
//!   (see [`crate::tenant_plane`]), so the user's recovery action takes
//!   effect on the next tick instead of waiting out stale backoff.
//! - **Layer 2, the circuit breaker.** **Only provider-touching executions
//!   feed [`ProviderBreaker`]** ([`WorkItem::touches_provider`]):
//!   embedding batches and llm work call the provider; maintenance tasks
//!   and (non-inline) feed polls do not, and their outcomes are neutral
//!   both ways — a draft-pipeline success says nothing about the
//!   provider's health and must not reset the streak or clear the
//!   detection window. Per provider-class execution: at most one
//!   rate-limit observation (one provider 429 can fan out across a batch's
//!   atoms — that's still one rate-limit response), an immediate credits
//!   pause on a 402, an immediate provider-kind pause on a 401/403
//!   credential rejection, and a streak reset on a failure-free
//!   provider-class execution.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use atomic_core::models::{AtomPipelineJobRequest, TaskRunTrigger};
use atomic_core::providers::{classify_provider_failure, ProviderFailureClass};
use atomic_core::scheduler::{runner, ScheduledTask, TaskContext};
use atomic_core::{ingest, reports, wiki, AtomicCore, EmbeddingEvent, TaskRun};
use atomic_server::event_bridge;
use atomic_server::state::ServerEvent;
use chrono::{DateTime, Utc};
use rand::Rng;
use tokio::task::JoinHandle;

use crate::account_cache::{AccountCache, TenantHandle};
use crate::backpressure::{BreakerConfig, ProviderBreaker, DEFAULT_RETRY_AFTER_CAP};
use crate::control_plane::ControlPlane;
use crate::dispatch_hints::{
    clear_hint_if_older, list_active_account_ids, list_hinted_accounts, mark_hint,
};
use crate::error::CloudError;
use crate::pools::{WorkClass, WorkTypeCap, WorkerPools, WorkerPoolsConfig};

pub use crate::backpressure::RATE_LIMIT_REQUEUE_DELAY;

/// `atom_pipeline_jobs.reason` stamped by the executor's layer-1 re-enqueue
/// of provider-classified failures. The provider-mutation effects path
/// re-arms exactly the rows carrying this marker
/// (`AtomicCore::rearm_pipeline_jobs`; see [`crate::tenant_plane`]).
pub const PROVIDER_BACKOFF_REASON: &str = "provider-backoff";

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
    /// mutating. The first tick after boot always full-scans; a failed full
    /// scan doesn't consume the interval (it retries next tick).
    pub slow_scan_interval: Duration,
    /// Ceiling on one tenant's ledger poll inside a tick. A wedged or
    /// unreachable tenant database must not head-of-line-block every other
    /// tenant's dispatch; a timed-out tenant is skipped for the tick (its
    /// hint is retained, so the next tick retries).
    pub tenant_poll_timeout: Duration,
    /// Jobs per pipeline-batch claim. One batch occupies one embedding-pool
    /// slot for its whole execution, so this trades per-claim overhead
    /// against fairness granularity.
    pub pipeline_batch_size: i32,
    /// Per-tenant in-flight cap for report runs — a work-type carve-out
    /// inside the llm class (plan table: reports per-tenant 1; see
    /// [`WorkTypeCap`]).
    pub reports_per_tenant_cap: usize,
    /// Ceiling on a provider-supplied `Retry-After` hint when scheduling
    /// layer-1 backoff (pipeline `not_before`, deferred `task_runs`
    /// horizons) — a hostile or buggy provider must not strand jobs.
    pub retry_after_cap: Duration,
    /// The four class pools' total / per-tenant caps.
    pub pools: WorkerPoolsConfig,
    /// Provider circuit-breaker tuning (window, threshold, cooldowns) for
    /// the [`ProviderBreaker`] the production executor feeds.
    pub breaker: BreakerConfig,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        Self {
            tick_interval: Duration::from_secs(2),
            slow_scan_interval: Duration::from_secs(300),
            tenant_poll_timeout: Duration::from_secs(10),
            pipeline_batch_size: 8,
            reports_per_tenant_cap: 1,
            retry_after_cap: DEFAULT_RETRY_AFTER_CAP,
            pools: WorkerPoolsConfig::default(),
            breaker: BreakerConfig::default(),
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

    /// Work-type-specific per-tenant carve-out (plan: reports = llm class
    /// with per-tenant cap 1). Counted against the work type's own
    /// in-flight, not the tenant's total llm in-flight — concurrent wiki
    /// work must not starve reports (see [`WorkTypeCap`]).
    fn work_type_cap(&self, config: &DispatcherConfig) -> Option<WorkTypeCap> {
        match self {
            WorkItem::Report { .. } => Some(WorkTypeCap {
                work_type: "report",
                per_tenant: config.reports_per_tenant_cap,
            }),
            _ => None,
        }
    }

    /// Whether executing this item performs provider (AI) work — the gate
    /// on circuit-breaker accounting (module docs: only provider-touching
    /// executions feed the breaker). Embedding batches and llm syntheses
    /// call the provider; maintenance tasks never do, and feed polls only
    /// fetch/parse/ingest — under the dispatcher composition their atom
    /// saves are enqueue-only, so the provider work happens in a later
    /// pipeline batch, not here.
    pub fn touches_provider(&self) -> bool {
        match self {
            WorkItem::PipelineBatch { .. }
            | WorkItem::WikiRegen { .. }
            | WorkItem::Report { .. } => true,
            WorkItem::SystemTask { .. } | WorkItem::FeedPoll { .. } => false,
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

/// One per-atom pipeline failure observed during a batch execution, with
/// its provider classification — the input to layer-1 re-enqueueing and
/// the breaker feed.
#[derive(Debug, Clone)]
struct PipelineFailure {
    atom_id: String,
    /// Which stage failed: `true` for embedding, `false` for tagging.
    embedding_stage: bool,
    class: ProviderFailureClass,
}

/// The production executor: resolves the tenant through the
/// [`AccountCache`], claims through the existing core machinery, bridges
/// events into the tenant's channel, and feeds provider backpressure
/// (module docs: "Provider backpressure").
pub struct CoreExecutor {
    cache: Arc<AccountCache>,
    breaker: Arc<ProviderBreaker>,
    /// Ceiling on provider `Retry-After` hints when re-enqueueing
    /// ([`DispatcherConfig::retry_after_cap`]).
    retry_after_cap: Duration,
}

impl CoreExecutor {
    pub fn new(
        cache: Arc<AccountCache>,
        breaker: Arc<ProviderBreaker>,
        retry_after_cap: Duration,
    ) -> Self {
        Self {
            cache,
            breaker,
            retry_after_cap,
        }
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

    /// Apply both backpressure layers after an execution settles: feed the
    /// breaker (at most one observation per failure class per execution —
    /// one provider response can fan out over a batch), re-enqueue
    /// classified pipeline failures with an honest `not_before`, and reset
    /// the streak on a clean run. Backpressure errors are logged, never
    /// propagated — the execution outcome is already settled in the durable
    /// ledgers.
    ///
    /// **Breaker accounting is gated on [`WorkItem::touches_provider`]**:
    /// outcomes of work that never calls the provider (maintenance, feed
    /// polls) are neutral in both directions. They carry no rate-limit
    /// evidence, and crediting their successes as "healthy" would let
    /// interleaved housekeeping hold the detection window below threshold
    /// forever and reset the escalation streak between trips (the
    /// adversarial finding this gate closes).
    async fn settle_backpressure(
        &self,
        account_id: &str,
        core: &AtomicCore,
        item: &WorkItem,
        outcome: &ExecOutcome,
        pipeline_failures: Vec<PipelineFailure>,
    ) {
        let mut rate_limited = false;
        let mut payment_required = false;
        let mut auth_failed = false;
        let mut observe = |class: &ProviderFailureClass| match class {
            ProviderFailureClass::RateLimited { .. } => rate_limited = true,
            ProviderFailureClass::PaymentRequired => payment_required = true,
            ProviderFailureClass::AuthFailed => auth_failed = true,
            ProviderFailureClass::Other => {}
        };
        for failure in &pipeline_failures {
            observe(&failure.class);
        }
        // task_runs failures surface as the item's outcome; the ledger has
        // already settled it (deferred for provider classes via the
        // installed policy, backed off otherwise — layer 1).
        if let ExecOutcome::Failed(error) = outcome {
            observe(&classify_provider_failure(error));
        }

        // Rate-limit first, then the immediate pause kinds: when one
        // execution somehow saw several, credits — which also gates the
        // interactive routes — must win the shared kind column last-write.
        if rate_limited {
            if let Err(e) = self.breaker.record_rate_limited(account_id).await {
                tracing::warn!(account_id, error = %e, "[dispatcher] breaker rate-limit record failed");
            }
        }
        let mut pause_until = None;
        if auth_failed {
            match self.breaker.record_auth_failed(account_id).await {
                Ok(until) => pause_until = until,
                Err(e) => {
                    tracing::warn!(account_id, error = %e, "[dispatcher] breaker auth record failed")
                }
            }
        }
        if payment_required {
            // OpenRouter's key-usage endpoint exposes no reset timestamp
            // (see provisioning_api::RuntimeKeyUsage), so the pause uses
            // the configured recheck horizon.
            match self.breaker.record_payment_required(account_id, None).await {
                Ok(until) => pause_until = until,
                Err(e) => {
                    tracing::warn!(account_id, error = %e, "[dispatcher] breaker credits record failed")
                }
            }
        }
        let provider_failure = rate_limited || payment_required || auth_failed;
        // record_healthy only for provider-class work that genuinely
        // succeeded; everything else is neutral (module docs).
        if item.touches_provider() && !provider_failure && matches!(outcome, ExecOutcome::Executed)
        {
            if let Err(e) = self.breaker.record_healthy(account_id).await {
                tracing::warn!(account_id, error = %e, "[dispatcher] breaker healthy record failed");
            }
        }

        if !pipeline_failures.is_empty() {
            if let Err(e) = self
                .requeue_pipeline_failures(core, pipeline_failures, pause_until)
                .await
            {
                tracing::warn!(
                    account_id,
                    error = %e,
                    "[dispatcher] re-enqueue of classified pipeline failures failed; \
                     the atoms stay status=failed until a manual retry"
                );
            }
        }
    }

    /// Layer 1 for the pipeline ledger: core settles a failed job as
    /// terminal (atom status `failed`, row cleared), so provider-classified
    /// failures — which WILL succeed later — are re-enqueued here with
    /// `not_before` pushed past the provider's horizon (`Retry-After`
    /// clamped to the configured cap; pause horizon for credits/auth). The
    /// enqueue re-derives stage flags from durable state: a failed
    /// embedding re-requests embedding (plus tagging iff the atom's tagging
    /// is still pending — never invent a tagging request the save path
    /// didn't make); a failed tagging re-requests tagging alone. One atom's
    /// read failure skips that atom (logged), never the rest of the batch.
    async fn requeue_pipeline_failures(
        &self,
        core: &AtomicCore,
        failures: Vec<PipelineFailure>,
        pause_until: Option<DateTime<Utc>>,
    ) -> Result<(), CloudError> {
        let now = Utc::now();
        let mut requests = Vec::new();
        for failure in failures {
            let not_before = match failure.class {
                ProviderFailureClass::RateLimited { retry_after_secs } => {
                    let delay = retry_after_secs
                        .map(|secs| Duration::from_secs(secs).min(self.retry_after_cap))
                        .unwrap_or(RATE_LIMIT_REQUEUE_DELAY);
                    now + chrono::Duration::from_std(delay).unwrap_or_default()
                }
                ProviderFailureClass::PaymentRequired | ProviderFailureClass::AuthFailed => {
                    pause_until.unwrap_or_else(|| {
                        now + chrono::Duration::from_std(RATE_LIMIT_REQUEUE_DELAY)
                            .unwrap_or_default()
                    })
                }
                // Genuine failures stay settled; retrying them is the
                // user's call (the existing retry routes).
                ProviderFailureClass::Other => continue,
            };
            let (embed_requested, tag_requested) = if failure.embedding_stage {
                let tagging_pending = match core.get_atom(&failure.atom_id).await {
                    Ok(found) => found
                        .map(|found| found.atom.tagging_status == "pending")
                        .unwrap_or(false),
                    Err(e) => {
                        // Skip-and-continue: one unreadable atom must not
                        // abandon the whole batch's re-enqueue. This atom
                        // stays status=failed until a manual retry.
                        tracing::warn!(
                            atom_id = failure.atom_id,
                            error = %e,
                            "[dispatcher] atom read failed during pipeline re-enqueue; skipping it"
                        );
                        continue;
                    }
                };
                (true, tagging_pending)
            } else {
                (false, true)
            };
            requests.push(AtomPipelineJobRequest {
                atom_id: failure.atom_id,
                embed_requested,
                tag_requested,
                not_before: Some(not_before.to_rfc3339()),
                reason: PROVIDER_BACKOFF_REASON.to_string(),
                replace_existing: false,
            });
        }
        if requests.is_empty() {
            return Ok(());
        }
        let count = requests.len();
        core.enqueue_pipeline_jobs(&requests)
            .await
            .map_err(CloudError::core("re-enqueueing backed-off pipeline jobs"))?;
        tracing::info!(
            count,
            "[dispatcher] re-enqueued provider-limited pipeline jobs"
        );
        Ok(())
    }
}

#[async_trait::async_trait]
impl WorkExecutor for CoreExecutor {
    async fn execute(&self, account_id: &str, item: &WorkItem) -> Result<ExecOutcome, CloudError> {
        let (core, handle) = self.resolve(account_id, item.db_id()).await?;
        let event_tx = handle.event_tx.clone();
        let mut pipeline_failures = Vec::new();
        let outcome = match item {
            WorkItem::PipelineBatch { batch, .. } => {
                // Per-job failures settle on the jobs themselves (status
                // columns + queue events) and never surface in the return
                // value — collect them off the event stream, classified,
                // for the backpressure pass below.
                let failures: Arc<Mutex<Vec<PipelineFailure>>> = Arc::default();
                let forward = event_bridge::embedding_event_callback(event_tx);
                let sink = Arc::clone(&failures);
                let claimed = core
                    .run_pipeline_jobs_batch(*batch, move |event: EmbeddingEvent| {
                        let observed = match &event {
                            EmbeddingEvent::EmbeddingFailed { atom_id, error } => {
                                Some((atom_id.clone(), true, error))
                            }
                            EmbeddingEvent::TaggingFailed { atom_id, error } => {
                                Some((atom_id.clone(), false, error))
                            }
                            _ => None,
                        };
                        if let Some((atom_id, embedding_stage, error)) = observed {
                            sink.lock()
                                .expect("failure sink poisoned")
                                .push(PipelineFailure {
                                    atom_id,
                                    embedding_stage,
                                    class: classify_provider_failure(error),
                                });
                        }
                        forward(event);
                    })
                    .await
                    .map_err(CloudError::core("running pipeline batch"))?;
                pipeline_failures =
                    std::mem::take(&mut *failures.lock().expect("failure sink poisoned"));
                // The batch as a unit "executed" iff the claim returned work.
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
        }?;

        self.settle_backpressure(account_id, &core, item, &outcome, pipeline_failures)
            .await;
        Ok(outcome)
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
    last_slow_scan: Mutex<Option<Instant>>,
}

impl Dispatcher {
    /// Production construction: real [`CoreExecutor`] over the same cache
    /// the serving stack uses (so workers publish into the channels live
    /// WebSocket clients hold), feeding a [`ProviderBreaker`] over the same
    /// control plane the pause gate reads.
    pub fn new(control: ControlPlane, cache: Arc<AccountCache>, config: DispatcherConfig) -> Self {
        let breaker = Arc::new(ProviderBreaker::new(
            control.clone(),
            config.breaker.clone(),
        ));
        let executor = Arc::new(CoreExecutor::new(
            Arc::clone(&cache),
            breaker,
            config.retry_after_cap,
        ));
        Self::with_executor(control, cache, config, executor)
    }

    /// Test seam: same dispatcher, custom executor.
    pub fn with_executor(
        control: ControlPlane,
        cache: Arc<AccountCache>,
        config: DispatcherConfig,
        executor: Arc<dyn WorkExecutor>,
    ) -> Self {
        Self {
            control,
            cache,
            pools: Arc::new(WorkerPools::new(config.pools)),
            executor,
            config,
            last_slow_scan: Mutex::new(None),
        }
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
            // Bounded per tenant: one wedged or unreachable tenant database
            // must not head-of-line-block every other tenant's dispatch.
            // Timeout and error both skip the tenant for this tick with its
            // hint retained — the next tick (or the slow scan) retries.
            let poll = match tokio::time::timeout(
                self.config.tenant_poll_timeout,
                self.poll_tenant(&account_id),
            )
            .await
            {
                Ok(Ok(p)) => p,
                Ok(Err(e)) => {
                    tracing::warn!(
                        account_id,
                        error = %e,
                        "[dispatcher] tenant poll failed; skipping this tick"
                    );
                    continue;
                }
                Err(_) => {
                    tracing::warn!(
                        account_id,
                        timeout_ms = self.config.tenant_poll_timeout.as_millis() as u64,
                        "[dispatcher] tenant poll timed out; skipping this tick"
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
                        item.work_type_cap(&self.config),
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

        if self.slow_scan_due() {
            // The marker advances only after the scan SUCCEEDS: a failed
            // full scan (the `?` below) retries on the next tick instead of
            // silently waiting out a whole interval.
            let active = list_active_account_ids(&self.control).await?;
            self.mark_slow_scan_done();
            for account_id in active {
                if seen.insert(account_id.clone()) {
                    candidates.push((account_id, None));
                }
            }
        }
        Ok(candidates)
    }

    /// Whether this tick should full-scan. The first tick after boot always
    /// full-scans — restart recovery should not wait out a whole interval.
    /// Read-only: the marker advances via [`Self::mark_slow_scan_done`],
    /// and only on success.
    fn slow_scan_due(&self) -> bool {
        self.last_slow_scan
            .lock()
            .expect("slow-scan marker poisoned")
            .map(|t| t.elapsed() >= self.config.slow_scan_interval)
            .unwrap_or(true)
    }

    /// Consume the slow-scan interval after a successful full scan.
    fn mark_slow_scan_done(&self) {
        *self
            .last_slow_scan
            .lock()
            .expect("slow-scan marker poisoned") = Some(Instant::now());
    }

    /// The tenant-pause gate (plan: per-tenant circuit breaker). The
    /// [`ProviderBreaker`] writes `accounts.provider_paused_until`
    /// (migration 007); both pause kinds hold background dispatch.
    /// Failures fail open with a warning — an unreadable pause column must
    /// not stop all dispatch.
    async fn paused_accounts(
        &self,
        candidates: &[(String, Option<DateTime<Utc>>)],
    ) -> HashSet<String> {
        if candidates.is_empty() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn marker_dispatcher(interval: Duration) -> Dispatcher {
        struct NoopExecutor;
        #[async_trait::async_trait]
        impl WorkExecutor for NoopExecutor {
            async fn execute(
                &self,
                _account_id: &str,
                _item: &WorkItem,
            ) -> Result<ExecOutcome, CloudError> {
                Ok(ExecOutcome::Skipped)
            }
        }
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .expect("lazy pool");
        let control = ControlPlane::from_pool_for_tests(pool);
        let cache = Arc::new(AccountCache::new(
            control.clone(),
            crate::provision::ClusterConfig {
                cluster_id: "test".to_string(),
                cluster_url: "postgres://unused:unused@127.0.0.1:1/unused".to_string(),
            },
            Arc::new(crate::keyvault::EnvMasterKeyVault::new([0u8; 32])),
            crate::account_cache::AccountCacheConfig::default(),
        ));
        Dispatcher::with_executor(
            control,
            cache,
            DispatcherConfig {
                slow_scan_interval: interval,
                ..DispatcherConfig::default()
            },
            Arc::new(NoopExecutor),
        )
    }

    /// The slow-scan interval is consumed only when a full scan SUCCEEDS:
    /// `slow_scan_due` is a pure read (a failed scan retries next tick),
    /// and `mark_slow_scan_done` is the success-path consumption.
    #[tokio::test]
    async fn slow_scan_marker_consumed_only_on_success() {
        let dispatcher = marker_dispatcher(Duration::from_secs(3600));

        // Boot: always due — and STILL due after a read (a failed scan
        // must not consume the interval).
        assert!(dispatcher.slow_scan_due());
        assert!(
            dispatcher.slow_scan_due(),
            "reading due-ness must not consume the interval"
        );

        // Success consumes it.
        dispatcher.mark_slow_scan_done();
        assert!(
            !dispatcher.slow_scan_due(),
            "a successful scan must start the interval"
        );

        // A short interval becomes due again after it elapses.
        let dispatcher = marker_dispatcher(Duration::from_millis(10));
        dispatcher.mark_slow_scan_done();
        assert!(!dispatcher.slow_scan_due());
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(dispatcher.slow_scan_due());
    }
}
