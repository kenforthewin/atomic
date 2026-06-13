//! Deferred graph maintenance for embeddings.
//!
//! Embedding writes mark the graph dirty. This task waits for the pipeline to
//! settle, then recomputes semantic edges and tag centroids in one place so
//! bulk imports and single-atom edits share the same maintenance path.
//!
//! Dispatched through `scheduler::runner`, which owns the `task_runs`
//! ledger claim, `last_run` advance, and event emission. This task
//! overrides [`ScheduledTask::is_due`] with its dirty-flag trigger:
//! it fires when the graph is dirty AND the pipeline is idle (or the
//! dirt has gone stale enough to force a pass anyway).

use crate::scheduler::{state as task_state, ScheduledTask, TaskContext, TaskError};
use crate::storage::StorageBackend;
use crate::{AtomicCore, AtomicCoreError};
use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::time::Duration;

pub struct GraphMaintenanceTask;

const TASK_ID: &str = "graph_maintenance";
const DEFAULT_INTERVAL: Duration = Duration::from_secs(15);
const DEFAULT_ENABLED: bool = true;
const DEFAULT_MAX_STALENESS_SECONDS: i64 = 300;
const EDGE_BATCH_SIZE: i32 = 500;

const DIRTY_SINCE_KEY: &str = "task.graph_maintenance.dirty_since";
const LAST_DIRTY_KEY: &str = "task.graph_maintenance.last_dirty_at";
const LAST_PROCESSED_KEY: &str = "task.graph_maintenance.last_processed_dirty_at";

#[derive(Debug, Clone)]
struct DirtyState {
    dirty_since: DateTime<Utc>,
    last_dirty_at: DateTime<Utc>,
}

#[async_trait]
impl ScheduledTask for GraphMaintenanceTask {
    fn id(&self) -> &'static str {
        TASK_ID
    }

    fn display_name(&self) -> &'static str {
        "Graph maintenance"
    }

    fn default_interval(&self) -> Duration {
        DEFAULT_INTERVAL
    }

    /// Dirty-flag trigger, not an interval: due when enabled, the graph
    /// has unprocessed dirt, and the embedding pipeline is idle (or the
    /// dirt has exceeded the staleness budget, forcing a pass even while
    /// jobs are still flowing). Storage errors read as "not due" — the
    /// next tick retries the check.
    async fn is_due(&self, core: &AtomicCore) -> bool {
        if !task_state::is_enabled(core, TASK_ID, DEFAULT_ENABLED).await {
            return false;
        }
        let dirty = match get_dirty_state(core.storage()).await {
            Ok(Some(d)) => d,
            Ok(None) | Err(_) => return false,
        };
        let active_pipeline_jobs = match core.storage().count_pipeline_jobs_sync().await {
            Ok(n) => n,
            Err(_) => return false,
        };
        let stale = Utc::now().signed_duration_since(dirty.dirty_since)
            >= ChronoDuration::seconds(max_staleness(core).await);
        active_pipeline_jobs == 0 || stale
    }

    async fn run(&self, core: &AtomicCore, _ctx: &TaskContext) -> Result<(), TaskError> {
        let result = execute(core).await?;
        tracing::info!(
            atoms = result.atoms_processed,
            edges = result.edges_written,
            tags = result.tags_recomputed,
            "[graph_maintenance] scheduler run complete"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct MaintenanceResult {
    atoms_processed: usize,
    edges_written: i32,
    tags_recomputed: usize,
}

pub async fn mark_dirty(storage: &StorageBackend) -> Result<(), AtomicCoreError> {
    let now = Utc::now().to_rfc3339();
    if get_dirty_state(storage).await?.is_none() {
        storage.set_setting_sync(DIRTY_SINCE_KEY, &now).await?;
    }
    storage.set_setting_sync(LAST_DIRTY_KEY, &now).await
}

pub async fn run_now(core: &AtomicCore) -> Result<(), AtomicCoreError> {
    execute(core).await?;
    task_state::set_last_run(core, TASK_ID, Utc::now()).await
}

/// Shared body of the scheduled and manual paths: snapshot the dirty
/// watermark, run maintenance, then mark everything up to that watermark
/// processed. Dirt marked *after* the snapshot stays pending, so a write
/// landing mid-run is picked up by the next pass rather than silently
/// swallowed.
async fn execute(core: &AtomicCore) -> Result<MaintenanceResult, AtomicCoreError> {
    let dirty = get_dirty_state(core.storage()).await?;
    let result = run_graph_maintenance(core).await?;
    if let Some(dirty) = dirty {
        core.storage()
            .set_setting_sync(LAST_PROCESSED_KEY, &dirty.last_dirty_at.to_rfc3339())
            .await?;
    }
    Ok(result)
}

async fn get_dirty_state(storage: &StorageBackend) -> Result<Option<DirtyState>, AtomicCoreError> {
    let settings = storage.get_all_settings_sync().await?;
    let Some(last_dirty_at) = settings
        .get(LAST_DIRTY_KEY)
        .and_then(|raw| parse_timestamp(raw))
    else {
        return Ok(None);
    };

    let last_processed = settings
        .get(LAST_PROCESSED_KEY)
        .and_then(|raw| parse_timestamp(raw));
    if last_processed
        .map(|processed| processed >= last_dirty_at)
        .unwrap_or(false)
    {
        return Ok(None);
    }

    let dirty_since = settings
        .get(DIRTY_SINCE_KEY)
        .and_then(|raw| parse_timestamp(raw))
        .unwrap_or(last_dirty_at);
    Ok(Some(DirtyState {
        dirty_since,
        last_dirty_at,
    }))
}

fn parse_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    if raw.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

async fn max_staleness(core: &AtomicCore) -> i64 {
    let settings = match core.storage().get_all_settings_sync().await {
        Ok(s) => s,
        Err(_) => return DEFAULT_MAX_STALENESS_SECONDS,
    };
    settings
        .get("task.graph_maintenance.max_staleness_seconds")
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_MAX_STALENESS_SECONDS)
}

async fn run_graph_maintenance(core: &AtomicCore) -> Result<MaintenanceResult, AtomicCoreError> {
    let storage = core.storage().clone();
    let mut processed_atom_ids = Vec::new();
    let mut edges_written = 0;
    let pending_edges = storage.count_pending_edges_sync().await?;

    tracing::info!(
        pending_edges,
        "[graph_maintenance] starting graph maintenance"
    );

    loop {
        let batch = storage.claim_pending_edges_sync(EDGE_BATCH_SIZE).await?;
        if batch.is_empty() {
            break;
        }

        let batch_edges = match storage
            .compute_semantic_edges_batch_sync(&batch, 0.5, 15)
            .await
        {
            Ok(count) => count,
            Err(e) => {
                tracing::error!(error = %e, "Failed to compute graph maintenance edge batch");
                0
            }
        };

        storage
            .set_edges_status_batch_sync(&batch, "complete")
            .await?;
        core.canvas_cache().invalidate_debounced();

        edges_written += batch_edges;
        processed_atom_ids.extend(batch);
        tracing::info!(
            batch_atoms = processed_atom_ids.len(),
            pending_edges,
            batch_edges,
            total_edges = edges_written,
            "[graph_maintenance] edge batch complete"
        );
        tokio::task::yield_now().await;
    }

    let affected_tag_ids = if processed_atom_ids.is_empty() {
        Vec::new()
    } else {
        storage
            .get_tag_ids_for_atoms_batch_impl(&processed_atom_ids)
            .await?
    };

    if !affected_tag_ids.is_empty() {
        storage
            .compute_tag_centroids_batch_impl(&affected_tag_ids)
            .await?;
    }

    if !processed_atom_ids.is_empty() {
        storage.rebuild_fts_index_sync().await?;
    }

    Ok(MaintenanceResult {
        atoms_processed: processed_atom_ids.len(),
        edges_written,
        tags_recomputed: affected_tag_ids.len(),
    })
}
