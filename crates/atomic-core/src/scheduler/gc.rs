//! Retention GC for the `task_runs` ledger (durable-task-runs phase 5).
//!
//! The ledger is append-per-invocation: with system tasks on a 15s tick,
//! feed polls per feed, and wiki regens per tag, unbounded growth is real —
//! and the desktop path has no ops and potentially very long uptime. This
//! task bounds it. It is itself a [`ScheduledTask`] dispatched through the
//! same `scheduler::runner` claim-and-record path as everything else, so GC
//! gets its own bounded run history for free. That dogfooding is also
//! self-protecting: while the sweep executes, its own ledger row is
//! `running` — non-terminal — and the policy never deletes non-terminal
//! rows, so GC can't eat the row that records it.
//!
//! Policy (see `docs/plans/durable-task-runs.md` §"Retention / cleanup"):
//!
//! - Non-terminal rows (`pending`, `running`) are live execution state and
//!   are never deleted.
//! - Per `(task_id, subject_id)`, the most recent `keep_per_subject`
//!   terminal rows are kept (default 50) — per-subject keying means each
//!   feed/tag retains its own recent history.
//! - Hard age cap: terminal rows older than `retain_days` (default 30) are
//!   eligible even inside the keep window.
//! - Exception: the most recent terminal *failure* per group is retained
//!   regardless of the above, up to `retain_failed_days` (default 90).
//!   Failures are rare and high-value; successes are noise.
//!
//! Deletes run in bounded batches with a yield in between — SQLite is
//! single-writer and a large backlog must not hold the write lock while
//! the user edits atoms. The eligibility SQL itself lives in the storage
//! backends (`TaskRunStore::gc_task_runs`); this module owns the knobs,
//! the batching loop, and the schedule.

use crate::error::AtomicCoreError;
use crate::scheduler::{ScheduledTask, TaskContext, TaskError};
use crate::AtomicCore;
use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
use std::collections::HashMap;
use std::time::Duration;

pub const TASK_ID: &str = "task_runs_gc";

/// Hourly by default — retention drift is slow, and the sweep competes
/// with user writes for the SQLite write lock. Overridable via
/// `task.task_runs_gc.interval_minutes` like every interval task.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(60 * 60);

const DEFAULT_KEEP_PER_SUBJECT: i32 = 50;
const DEFAULT_RETAIN_DAYS: i64 = 30;
const DEFAULT_RETAIN_FAILED_DAYS: i64 = 90;

/// Rows deleted per storage round-trip. Small enough that one batch is a
/// short write transaction on the desktop path; the loop yields between
/// batches so an editing user gets the lock back.
pub const DELETE_BATCH_SIZE: i32 = 500;

/// The retention knobs, resolved from per-DB settings with the plan's
/// defaults as fallbacks. Settings follow the `task.{id}.*` convention:
///
/// - `task.task_runs_gc.keep_per_subject` (default 50)
/// - `task.task_runs_gc.retain_days` (default 30)
/// - `task.task_runs_gc.retain_failed_days` (default 90)
///
/// (`task.task_runs_gc.enabled` / `.interval_minutes` ride the generic
/// [`ScheduledTask::is_due`] gate via `scheduler::state`, not this struct.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub keep_per_subject: i32,
    pub retain_days: i64,
    pub retain_failed_days: i64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            keep_per_subject: DEFAULT_KEEP_PER_SUBJECT,
            retain_days: DEFAULT_RETAIN_DAYS,
            retain_failed_days: DEFAULT_RETAIN_FAILED_DAYS,
        }
    }
}

impl RetentionPolicy {
    /// Resolve the policy from the per-DB settings table. Reads go through
    /// `core.storage()` directly — never `core.get_settings()`, which
    /// routes to the shared registry when one is attached (multi-DB
    /// gotcha). No seed rows exist: a fresh DB's empty settings table
    /// yields the defaults, and missing / unparseable / non-positive
    /// values fall back per-knob.
    ///
    /// A storage *read error* propagates instead of defaulting: the
    /// defaults may be tighter than the operator's configured overrides,
    /// so failing open here would delete more history than asked. The
    /// caller skips the sweep instead (see [`TaskRunsGcTask::run`]).
    pub async fn load(core: &AtomicCore) -> Result<Self, AtomicCoreError> {
        let settings = core.storage().get_all_settings_sync().await?;
        let defaults = Self::default();
        Ok(Self {
            keep_per_subject: parse_positive(&settings, "keep_per_subject")
                .and_then(|v| i32::try_from(v).ok())
                .unwrap_or(defaults.keep_per_subject),
            retain_days: parse_positive(&settings, "retain_days").unwrap_or(defaults.retain_days),
            retain_failed_days: parse_positive(&settings, "retain_failed_days")
                .unwrap_or(defaults.retain_failed_days),
        })
    }
}

/// Read `task.task_runs_gc.{field}` as a positive integer. Zero and
/// negative values are rejected, not honored — `keep_per_subject = 0` or
/// `retain_days = 0` would mean "delete history as it lands", which is
/// never what a typo'd override intends.
fn parse_positive(settings: &HashMap<String, String>, field: &str) -> Option<i64> {
    settings
        .get(&format!("task.{TASK_ID}.{field}"))
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
}

/// What one full sweep did — surfaced for logs and asserted by tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcOutcome {
    pub deleted: u64,
    pub batches: u32,
}

/// Delete every eligible terminal row in bounded batches of `batch_size`,
/// yielding between batches. The cutoffs are snapshotted once at the start
/// of the sweep; ranks are recomputed by the storage layer per batch, and
/// deleting oldest-first never promotes a surviving row into eligibility,
/// so the loop converges on exactly the rows the snapshot deemed eligible.
pub async fn sweep(
    core: &AtomicCore,
    policy: &RetentionPolicy,
    batch_size: i32,
) -> Result<GcOutcome, AtomicCoreError> {
    let now = Utc::now();
    let age_cutoff = (now - ChronoDuration::days(policy.retain_days)).to_rfc3339();
    let failed_cutoff = (now - ChronoDuration::days(policy.retain_failed_days)).to_rfc3339();

    let mut outcome = GcOutcome::default();
    loop {
        let deleted = core
            .storage()
            .gc_task_runs_sync(
                policy.keep_per_subject,
                &age_cutoff,
                &failed_cutoff,
                batch_size,
            )
            .await?;
        if deleted == 0 {
            break;
        }
        outcome.deleted += deleted;
        outcome.batches += 1;
        if deleted < batch_size as u64 {
            break;
        }
        // SQLite is single-writer: hand the lock back between batches so a
        // user editing atoms isn't stuck behind a long backlog drain.
        tokio::task::yield_now().await;
    }
    Ok(outcome)
}

/// The scheduled wrapper: hourly tick, default-on, policy loaded fresh per
/// run so settings changes apply without a restart.
pub struct TaskRunsGcTask;

#[async_trait]
impl ScheduledTask for TaskRunsGcTask {
    fn id(&self) -> &'static str {
        TASK_ID
    }

    fn display_name(&self) -> &'static str {
        "Task run retention GC"
    }

    fn default_interval(&self) -> Duration {
        DEFAULT_INTERVAL
    }

    // is_due: trait default — enabled (default true) AND the configured
    // interval elapsed since the last successful sweep.

    async fn run(&self, core: &AtomicCore, _ctx: &TaskContext) -> Result<(), TaskError> {
        // Fail closed: if the knobs can't be read, skip the sweep entirely
        // rather than deleting under the (possibly tighter-than-configured)
        // defaults. Returning the error settles it on the ledger row, so
        // the skipped sweep is visible in run history and retried with
        // backoff instead of silently waiting out the next interval.
        let policy = match RetentionPolicy::load(core).await {
            Ok(policy) => policy,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "[task_runs_gc] retention settings unreadable; skipping sweep"
                );
                return Err(e.into());
            }
        };
        let outcome = sweep(core, &policy, DELETE_BATCH_SIZE).await?;
        if outcome.deleted > 0 {
            tracing::info!(
                deleted = outcome.deleted,
                batches = outcome.batches,
                keep_per_subject = policy.keep_per_subject,
                retain_days = policy.retain_days,
                retain_failed_days = policy.retain_failed_days,
                "[task_runs_gc] retention sweep complete"
            );
        }
        Ok(())
    }
}
