//! Scheduled-task framework for atomic-core.
//!
//! This module defines a minimal scheduling primitive that any transport
//! (atomic-server, Tauri sidecar, etc.) can drive from its own runtime. The
//! registry lives here so task implementations ship with core, while the
//! ticking loop itself is owned by the caller.
//!
//! Tasks own their work and their due-ness predicate; the [`runner`] module
//! owns the execution lifecycle — `task_runs` ledger claim, retry/backoff,
//! the `last_run` fast-path advance, and event emission. See
//! [`runner::run_task`] for the claim-and-record contract.

pub mod ledger;
pub mod runner;
pub mod state;

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

/// A unit of work that runs on a schedule.
///
/// Implementations are registered in a [`TaskRegistry`] and dispatched by
/// [`runner::run_task`], which owns the full execution lifecycle: the
/// `is_due` gate, the `task_runs` ledger claim (durable lease + retry +
/// backoff), the `last_run` fast-path advance on terminal success, and
/// [`TaskEvent`] emission. A task implementation only supplies the due-ness
/// predicate and the work itself.
#[async_trait]
pub trait ScheduledTask: Send + Sync {
    /// Stable identifier used as the key for per-task state in the settings table.
    fn id(&self) -> &'static str;

    /// Human-readable name for logs and future UI.
    fn display_name(&self) -> &'static str;

    /// Default interval between runs when the per-task setting is absent.
    fn default_interval(&self) -> Duration;

    /// Cheap pre-claim gate, evaluated every tick for every database. The
    /// default covers interval tasks: enabled AND the configured interval
    /// has elapsed since the last *successful* run. Override for tasks with
    /// richer triggers (e.g. dirty-flag tasks).
    ///
    /// This is the hot path (N tasks × N databases per tick) — it must stay
    /// settings-table-cheap and never query `task_runs`; the ledger is only
    /// consulted after this returns `true`.
    async fn is_due(&self, core: &crate::AtomicCore) -> bool {
        state::is_due(core, self.id(), self.default_interval(), true).await
    }

    /// Execute the task. Runs only after [`Self::is_due`] passed and the
    /// runner claimed a `task_runs` row. Return `Err` to let the ledger
    /// schedule a backed-off retry; do not advance `last_run` here — the
    /// runner does that on success.
    async fn run(&self, core: &crate::AtomicCore, ctx: &TaskContext) -> Result<(), TaskError>;
}

/// Context passed to each task run. Currently just a callback sink so tasks
/// can emit events without knowing about the host transport.
#[derive(Clone)]
pub struct TaskContext {
    pub event_cb: Arc<dyn Fn(TaskEvent) + Send + Sync>,
    pub embedding_event_cb: Arc<dyn Fn(crate::EmbeddingEvent) + Send + Sync>,
}

/// Events emitted by scheduled tasks. The host runtime adapts these into its
/// own event channel (see `atomic-server::event_bridge::task_event_callback`).
#[derive(Debug, Clone)]
pub enum TaskEvent {
    Started {
        task_id: String,
        db_id: String,
    },
    Completed {
        task_id: String,
        db_id: String,
        /// Identifier of the resource produced by the run, if any (e.g. a
        /// briefing id). Lets downstream UIs deep-link to the result.
        result_id: Option<String>,
    },
    Failed {
        task_id: String,
        db_id: String,
        error: String,
    },
}

/// Errors returned by [`ScheduledTask::run`]. By the time `run` executes,
/// the runner has already settled enablement and due-ness, so every error
/// here is a genuine failure — it lands on the `task_runs` row as
/// `last_error` and drives the retry/backoff decision.
#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error("{0}")]
    Other(String),
}

impl From<crate::AtomicCoreError> for TaskError {
    fn from(e: crate::AtomicCoreError) -> Self {
        TaskError::Other(e.to_string())
    }
}

/// Registry of scheduled tasks. Owns the task trait objects and the
/// per-(task, database) lock map.
///
/// The in-memory lock is a *fast-path optimization*, not a correctness
/// guard: it lets a tick skip a task this process is already running
/// without a storage round-trip. The durable `lease_until` on the task's
/// `task_runs` row is the source of truth for "already running" — it also
/// covers peers in other processes and survives restarts, which this map
/// never did.
pub struct TaskRegistry {
    tasks: Vec<Arc<dyn ScheduledTask>>,
    /// Per-task-per-database locks. A task that's still running when the next
    /// tick arrives must be skipped, not queued.
    locks: Mutex<HashMap<(String, String), Arc<AsyncMutex<()>>>>,
}

impl Default for TaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn register(&mut self, task: Arc<dyn ScheduledTask>) {
        self.tasks.push(task);
    }

    pub fn tasks(&self) -> &[Arc<dyn ScheduledTask>] {
        &self.tasks
    }

    /// Try to acquire the per-(task, db) lock. Returns `None` if the lock is
    /// already held (task still running from a previous tick).
    pub fn try_lock(&self, task_id: &str, db_id: &str) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        let lock = {
            let mut map = self.locks.lock().expect("scheduler locks mutex poisoned");
            map.entry((task_id.to_string(), db_id.to_string()))
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        lock.try_lock_owned().ok()
    }
}
