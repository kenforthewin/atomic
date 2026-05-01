//! Weekly GC of `health_dismissals` entries.
//!
//! Deletes rows whose `expires_at` has passed, and rows that reference
//! atoms/tags that no longer exist. Safe to re-run; idempotent.

use crate::scheduler::{state as task_state, ScheduledTask, TaskContext, TaskError, TaskEvent};
use crate::AtomicCore;
use async_trait::async_trait;
use std::time::Duration;

pub struct DismissalGcTask;

const TASK_ID: &str = "health_dismissal_gc";
const DEFAULT_INTERVAL: Duration = Duration::from_secs(7 * 24 * 60 * 60); // 7 days
const DEFAULT_ENABLED: bool = true;

#[async_trait]
impl ScheduledTask for DismissalGcTask {
    fn id(&self) -> &'static str {
        TASK_ID
    }

    fn display_name(&self) -> &'static str {
        "Health dismissal GC"
    }

    fn default_interval(&self) -> Duration {
        DEFAULT_INTERVAL
    }

    async fn run(&self, core: &AtomicCore, ctx: &TaskContext) -> Result<(), TaskError> {
        if !task_state::is_enabled(core, TASK_ID, DEFAULT_ENABLED).await {
            return Err(TaskError::Disabled);
        }
        if !task_state::is_due(core, TASK_ID, DEFAULT_INTERVAL, DEFAULT_ENABLED).await {
            return Err(TaskError::NotDue);
        }

        let db_id = core
            .db_path()
            .file_stem()
            .and_then(|s| s.to_str())
            .map(String::from)
            .unwrap_or_else(|| "default".to_string());

        (ctx.event_cb)(TaskEvent::Started {
            task_id: TASK_ID.to_string(),
            db_id: db_id.clone(),
        });

        match core.storage().gc_dismissals_sync().await {
            Ok(removed) => {
                tracing::info!(removed, db_id = %db_id, "[dismissal_gc] cleanup complete");
                task_state::set_last_run(core, TASK_ID, chrono::Utc::now())
                    .await
                    .ok();
                (ctx.event_cb)(TaskEvent::Completed {
                    task_id: TASK_ID.to_string(),
                    db_id,
                    result_id: None,
                });
                Ok(())
            }
            Err(e) => {
                let msg = e.to_string();
                (ctx.event_cb)(TaskEvent::Failed {
                    task_id: TASK_ID.to_string(),
                    db_id,
                    error: msg.clone(),
                });
                Err(TaskError::Other(msg))
            }
        }
    }
}
