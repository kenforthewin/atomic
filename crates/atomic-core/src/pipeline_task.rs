//! Draft pipeline scheduled task.
//!
//! Picks up autosaved atoms whose content is durable but whose AI pipeline
//! has not been explicitly finalized by the foreground client.
//!
//! Dispatched through `scheduler::runner`, which owns the `task_runs`
//! ledger claim, `last_run` advance, and event emission — this type only
//! supplies the work. Due-ness is the trait default: enabled + interval
//! elapsed since the last successful run.

use crate::scheduler::{ScheduledTask, TaskContext, TaskError};
use crate::AtomicCore;
use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
use std::sync::Arc;
use std::time::Duration;

pub struct DraftPipelineTask;

const TASK_ID: &str = "draft_pipeline";
const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);
const DEFAULT_QUIET_MINUTES: i64 = 1;

#[async_trait]
impl ScheduledTask for DraftPipelineTask {
    fn id(&self) -> &'static str {
        TASK_ID
    }

    fn display_name(&self) -> &'static str {
        "Draft pipeline"
    }

    fn default_interval(&self) -> Duration {
        DEFAULT_INTERVAL
    }

    async fn run(&self, core: &AtomicCore, ctx: &TaskContext) -> Result<(), TaskError> {
        let quiet_minutes = quiet_minutes(core).await;
        let cutoff = Utc::now() - ChronoDuration::minutes(quiet_minutes);
        let on_event = {
            let cb = Arc::clone(&ctx.embedding_event_cb);
            move |event| cb(event)
        };

        let queued_count = core
            .process_pending_embeddings_due(cutoff, on_event)
            .await
            .map_err(TaskError::from)?;

        tracing::info!(
            quiet_minutes,
            queued_count,
            "[draft_pipeline] scheduler run complete"
        );

        Ok(())
    }
}

async fn quiet_minutes(core: &AtomicCore) -> i64 {
    let settings = match core.storage().get_all_settings_sync().await {
        Ok(s) => s,
        Err(_) => return DEFAULT_QUIET_MINUTES,
    };
    settings
        .get("task.draft_pipeline.quiet_minutes")
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|minutes| *minutes > 0)
        .unwrap_or(DEFAULT_QUIET_MINUTES)
}
