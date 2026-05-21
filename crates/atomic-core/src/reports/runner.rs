//! Full lifecycle for a single report run.
//!
//! Composed around [`crate::scheduler::ledger::claim_or_create`] and the
//! storage-level transactional finding-write helper:
//!
//! 1. Claim a `task_runs` row (or reuse a runnable one).
//! 2. Resolve source + context scopes.
//! 3. Empty source scope → ledger complete with `result_id = None`, cache
//!    advance, no finding atom written.
//! 4. Non-empty scope → agent loop → transactional write of atom +
//!    provenance + citations → ledger complete with `result_id =
//!    finding_atom_id`.
//! 5. Any error → ledger fail (router decides retry vs abandon) and
//!    cache `last_error` update.
//!
//! `RunOutcome` is the public surface; callers (REST handler, scheduler
//! tick) don't need to know about the ledger primitives directly.

use crate::error::AtomicCoreError;
use crate::models::{Report, ReportFinding, ReportFindingCitation, TaskRunTrigger};
use crate::reports::{agentic, scope};
use crate::scheduler::ledger;
use crate::AtomicCore;
use crate::CreateAtomRequest;
use chrono::Utc;

/// Terminal outcome of a single report run. Mirrors the three ways the
/// ledger row can settle for callers that want a structured value back.
#[derive(Debug, Clone)]
pub enum RunOutcome {
    /// Agent ran and wrote a finding atom. Holds the atom id.
    Succeeded { finding_atom_id: String },
    /// Scope was empty at this tick — no atom written, no LLM call.
    /// Reason is a short human-readable string the cache can surface.
    EmptyScope { reason: String },
    /// Run failed and the ledger took the retry-or-abandon decision.
    /// `attempts_so_far` is the count post-claim; the caller can read
    /// it from the eventual `task_runs` row if they need it for UI.
    Failed { error: String },
    /// `claim_or_create` returned `None`: another worker is already
    /// running this report, or the row's `next_attempt_at` is in the
    /// future. Caller should skip this tick.
    Skipped,
}

/// Entry point for both scheduled and manual runs. `trigger` lands on
/// the `task_runs` row so history queries can distinguish them.
pub async fn run_report(
    core: &AtomicCore,
    report: &Report,
    trigger: TaskRunTrigger,
) -> Result<RunOutcome, AtomicCoreError> {
    // Run id space is per-task. We use `report::{id}` so the scheduler's
    // claim contention is scoped to this report and concurrent runs of
    // *different* reports don't serialize against each other.
    let task_id = format!("report::{}", report.id);
    // Three attempts is the default ledger contract — phase 4 may expose
    // it per-report; phase 2 ships with the default.
    let handle = match ledger::claim_or_create(core, &task_id, None, trigger, 3).await? {
        Some(h) => h,
        None => return Ok(RunOutcome::Skipped),
    };

    match execute(core, report, &handle).await {
        Ok(RunInner::Empty { reason, watermark }) => {
            // Empty scope is terminal-success: advance cache to the
            // resolution-time watermark (not now), complete the ledger
            // row without a result_id. The agent never ran; any prior
            // `last_error` is left in place (a transient empty-scope
            // tick shouldn't clear a real failure record).
            core.storage()
                .update_report_cache_sync(&report.id, Some(&watermark), None, None)
                .await?;
            let _ = handle.complete(None).await?;
            Ok(RunOutcome::EmptyScope { reason })
        }
        Ok(RunInner::Written { atom_id, watermark }) => {
            core.storage()
                .update_report_cache_sync(
                    &report.id,
                    Some(&watermark),
                    Some(Some(atom_id.as_str())),
                    Some(None),
                )
                .await?;
            let _ = handle.complete(Some(atom_id.clone())).await?;
            Ok(RunOutcome::Succeeded {
                finding_atom_id: atom_id,
            })
        }
        Err(e) => {
            let err_str = e.to_string();
            // Stamp the failure on the cache *without* touching
            // `last_run_at`. A first-run failure has `last_run_at = None`
            // — writing an empty string here would make subsequent ticks
            // fail RFC3339 parsing in `schedule::is_due` and silently
            // wedge the report. Leaving the column unchanged means the
            // schedule continues to anchor on its previous value (or
            // first-run anchor for never-succeeded reports).
            let _ = core
                .storage()
                .update_report_cache_sync(&report.id, None, None, Some(Some(err_str.as_str())))
                .await;
            let _ = handle.fail(err_str.clone()).await?;
            Ok(RunOutcome::Failed { error: err_str })
        }
    }
}

/// Inner result before we map it to a `RunOutcome` + cache writes. Both
/// branches carry the **scope-resolution timestamp** rather than letting
/// the caller stamp `Utc::now()` at completion. For
/// `source_scope_window = SinceLastRun`, the next tick filters atoms with
/// `created_at > last_run_at`; if we advanced past completion time
/// instead, any atom captured between scope resolution and run finish
/// would have `created_at` in the just-crossed gap and never be picked up
/// on the following run.
enum RunInner {
    Empty { reason: String, watermark: String },
    Written { atom_id: String, watermark: String },
}

async fn execute(
    core: &AtomicCore,
    report: &Report,
    handle: &ledger::RunHandle,
) -> Result<RunInner, AtomicCoreError> {
    let now = Utc::now();
    // Watermark for `last_run_at` is the moment we resolved scope. Any
    // atom captured between here and the run's completion belongs to the
    // *next* batch, not this one; advancing past completion time would
    // silently swallow it.
    let watermark = now.to_rfc3339();

    let source = scope::resolve_source(core, report, now).await?;
    if source.atoms.is_empty() {
        let reason = if source.total_in_scope == 0 {
            "no atoms matched the configured source scope".to_string()
        } else {
            format!(
                "{} atoms in scope but post-filter/cap left none",
                source.total_in_scope
            )
        };
        return Ok(RunInner::Empty { reason, watermark });
    }

    let ctx_filter = scope::build_context_filter(core, report, &source, now).await?;

    let output = agentic::run(
        core,
        report,
        &source.atoms,
        source.total_in_scope,
        &ctx_filter,
    )
    .await?;

    // Empty-content guard: if the agent returned no prose, treat as
    // empty-scope rather than writing a blank atom. This shouldn't
    // happen with a well-behaved LLM but the cost of catching it is one
    // string check.
    if output.content.trim().is_empty() {
        return Ok(RunInner::Empty {
            reason: "agent returned empty content".to_string(),
            watermark,
        });
    }

    let atom_id = uuid::Uuid::new_v4().to_string();
    // The atom's own `created_at` records when the run finished — distinct
    // from the report-watermark, which records the cutoff for what was
    // *in scope* for this run.
    let created_at = Utc::now().to_rfc3339();
    let provenance = ReportFinding {
        finding_atom_id: atom_id.clone(),
        report_id: Some(report.id.clone()),
        run_id: Some(handle.run().id.clone()),
        report_name_snapshot: report.name.clone(),
        created_at: created_at.clone(),
    };
    let citations: Vec<ReportFindingCitation> = output
        .citations
        .iter()
        .map(|c| ReportFindingCitation {
            finding_atom_id: atom_id.clone(),
            cited_atom_id: c.cited_atom_id.clone(),
            position: c.position,
            excerpt: c.excerpt.clone(),
        })
        .collect();

    let atom_request = CreateAtomRequest {
        content: output.content.clone(),
        source_url: None,
        published_at: None,
        tag_ids: report.output_atom_tags.clone(),
        skip_if_source_exists: false,
    };

    core.storage()
        .write_finding_transactionally_sync(
            &atom_request,
            &atom_id,
            &created_at,
            &provenance,
            &citations,
        )
        .await?;

    Ok(RunInner::Written { atom_id, watermark })
}
