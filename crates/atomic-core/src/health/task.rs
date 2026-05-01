//! Nightly health maintenance scheduled task.
//!
//! Runs daily at ~3 AM (configurable).  Automatically applies Safe + Low tier
//! fixes and records the health report for trending.  If the score drops below
//! 70, the next briefing run will include a health summary.

use crate::health::{self, FixRequest};
use crate::scheduler::{state as task_state, ScheduledTask, TaskContext, TaskError, TaskEvent};
use crate::AtomicCore;
use async_trait::async_trait;
use std::time::Duration;

pub struct HealthMaintenanceTask;

const TASK_ID: &str = "health_maintenance";
const DEFAULT_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const DEFAULT_ENABLED: bool = true;

#[async_trait]
impl ScheduledTask for HealthMaintenanceTask {
    fn id(&self) -> &'static str {
        TASK_ID
    }

    fn display_name(&self) -> &'static str {
        "Knowledge health maintenance"
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
            .map(|s| s.to_string())
            .unwrap_or_else(|| "default".to_string());

        (ctx.event_cb)(TaskEvent::Started {
            task_id: TASK_ID.to_string(),
            db_id: db_id.clone(),
        });

        // Run health check
        let report = match health::compute_health(core).await {
            Ok(r) => r,
            Err(e) => {
                let msg = e.to_string();
                (ctx.event_cb)(TaskEvent::Failed {
                    task_id: TASK_ID.to_string(),
                    db_id,
                    error: msg.clone(),
                });
                return Err(TaskError::Other(msg));
            }
        };

        let score_before = report.overall_score;
        tracing::info!(
            score = score_before,
            status = %report.overall_status,
            "[health_maintenance] initial score"
        );

        // Auto-fix Safe + Low tier issues
        let fix_req = FixRequest {
            checks: None,
            mode: "auto".to_string(),
            include_medium: false,
        };

        match health::run_fix(core, &fix_req).await {
            Ok(fix_resp) => {
                tracing::info!(
                    fixes = fix_resp.actions_taken.len(),
                    new_score = fix_resp.new_score,
                    "[health_maintenance] fixes applied"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "[health_maintenance] fix run failed");
            }
        }

        // Persist last_run
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
}
