//! Claim-and-record dispatch for [`ScheduledTask`]s.
//!
//! This is the system-task analog of `crate::reports::runner`: every firing
//! of a scheduled task becomes a `task_runs` row claimed through
//! [`crate::scheduler::ledger::claim_or_create`], executed, and transitioned
//! terminally. That replaces the old spawn-and-forget tick with durable
//! retry + exponential backoff + visible run history, and it fixes the
//! latent retry-storm bug: a failing task used to re-run on every 15s tick
//! forever because nothing but `last_run` (which only advances on success)
//! throttled it. Now the claim path gates on the row's `next_attempt_at`,
//! so failures wait out their backoff window.
//!
//! Division of responsibility:
//!
//! - [`ScheduledTask::is_due`] — cheap per-tick gate over the per-DB
//!   settings table. Never touches `task_runs`.
//! - [`run_task`] — ledger claim, execution, `last_run` advance on success,
//!   terminal transition, [`TaskEvent`] emission.
//! - [`tick_all_databases`] — the per-DB fan-out main loops drive on a
//!   timer; extracted here so tests can drive ticks directly.

use crate::error::AtomicCoreError;
use crate::models::TaskRunTrigger;
use crate::scheduler::{ledger, state, ScheduledTask, TaskContext, TaskEvent, TaskRegistry};
use crate::{AtomicCore, DatabaseManager};
use chrono::Utc;
use std::sync::Arc;
use tokio::task::JoinHandle;

/// Retry budget stamped on freshly inserted rows. Same default contract as
/// the reports runner; per-task overrides can come later if a task needs
/// them.
pub const DEFAULT_MAX_ATTEMPTS: i32 = 3;

/// Terminal outcome of dispatching one `(task, database)` pair. Mirrors
/// `crate::reports::RunOutcome` so callers branch on a structured value
/// instead of parsing errors.
#[derive(Debug, Clone)]
pub enum DispatchOutcome {
    /// The task ran and the ledger row settled `succeeded`; `last_run` was
    /// advanced.
    Succeeded,
    /// The task ran and failed; the ledger took the retry-or-abandon
    /// decision and `last_run` was left untouched.
    Failed { error: String },
    /// `claim_or_create` returned `None`: another worker holds a live
    /// lease, or a pending row is still inside its backoff window. Skip
    /// this tick.
    Skipped,
    /// [`ScheduledTask::is_due`] returned false — no ledger row was
    /// created or touched.
    NotDue,
}

/// Run one scheduled task against one database through the ledger.
///
/// `db_id` is only used to label emitted [`TaskEvent`]s; the storage all
/// flows through `core`, which is already bound to the right database.
pub async fn run_task(
    core: &AtomicCore,
    db_id: &str,
    task: &dyn ScheduledTask,
    ctx: &TaskContext,
) -> Result<DispatchOutcome, AtomicCoreError> {
    if !task.is_due(core).await {
        return Ok(DispatchOutcome::NotDue);
    }

    // System tasks are singletons per database, so no subject_id. The claim
    // is the durable re-entry guard: a live lease (this process or a peer)
    // or an unexpired backoff window both come back as `None`.
    let handle = match ledger::claim_or_create(
        core,
        task.id(),
        None,
        TaskRunTrigger::Schedule,
        DEFAULT_MAX_ATTEMPTS,
    )
    .await?
    {
        Some(h) => h,
        None => return Ok(DispatchOutcome::Skipped),
    };

    (ctx.event_cb)(TaskEvent::Started {
        task_id: task.id().to_string(),
        db_id: db_id.to_string(),
    });

    match task.run(core, ctx).await {
        Ok(()) => {
            // Settle the ledger row first, then advance the definition
            // fast-path — and *only* on terminal success: `is_due` keys off
            // `last_run`, so a failure leaves the task due and the ledger
            // row's `next_attempt_at` alone decides when to retry. In this
            // order a `set_last_run` error can't strand a succeeded run in
            // `running` until its lease expires and gets re-executed.
            let _ = handle.complete(None).await?;
            state::set_last_run(core, task.id(), Utc::now()).await?;
            (ctx.event_cb)(TaskEvent::Completed {
                task_id: task.id().to_string(),
                db_id: db_id.to_string(),
                result_id: None,
            });
            Ok(DispatchOutcome::Succeeded)
        }
        Err(e) => {
            let error = e.to_string();
            let _ = handle.fail(error.clone()).await?;
            (ctx.event_cb)(TaskEvent::Failed {
                task_id: task.id().to_string(),
                db_id: db_id.to_string(),
                error: error.clone(),
            });
            Ok(DispatchOutcome::Failed { error })
        }
    }
}

/// One scheduler tick over every data database: for each `(task, db)` pair,
/// take the in-memory fast-path lock and spawn [`run_task`].
///
/// Returns the spawned `JoinHandle`s so callers choose their own join
/// semantics: the production loop drops them (a slow task must not stall
/// the next tick — the lease and lock keep re-entry safe), while tests
/// await them to drive ticks deterministically.
pub async fn tick_all_databases(
    manager: &DatabaseManager,
    registry: &Arc<TaskRegistry>,
    ctx: &TaskContext,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();
    let databases = match manager.list_databases().await {
        Ok((dbs, _)) => dbs,
        Err(e) => {
            tracing::warn!(error = %e, "[scheduler] list_databases failed; skipping tick");
            return handles;
        }
    };
    for db_info in &databases {
        let core = match manager.get_core(&db_info.id).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(db = %db_info.id, error = %e, "[scheduler] get_core failed");
                continue;
            }
        };
        for task in registry.tasks() {
            // Fast-path: this process already has the task in flight for
            // this DB. Purely an optimization — without it the ledger's
            // live lease would reject the claim anyway.
            let Some(guard) = registry.try_lock(task.id(), &db_info.id) else {
                continue;
            };
            let task = Arc::clone(task);
            let core = core.clone();
            let ctx = ctx.clone();
            let db_id = db_info.id.clone();
            handles.push(tokio::spawn(async move {
                match run_task(&core, &db_id, task.as_ref(), &ctx).await {
                    Ok(DispatchOutcome::Succeeded) => {
                        tracing::info!(task = task.id(), db = %db_id, "[scheduler] task succeeded");
                    }
                    Ok(DispatchOutcome::Failed { error }) => {
                        tracing::warn!(
                            task = task.id(),
                            db = %db_id,
                            error = %error,
                            "[scheduler] task failed; ledger scheduled retry or abandoned"
                        );
                    }
                    Ok(DispatchOutcome::Skipped) | Ok(DispatchOutcome::NotDue) => {}
                    Err(e) => {
                        tracing::warn!(
                            task = task.id(),
                            db = %db_id,
                            error = %e,
                            "[scheduler] task dispatch errored"
                        );
                    }
                }
                drop(guard);
            }));
        }
    }
    handles
}
