//! Ledger-dispatched wiki regeneration — durable-task-runs phase 4.
//!
//! The tag is the *definition*; every regeneration becomes a `task_runs`
//! row with `task_id = `[`WIKI_REGENERATE_TASK_ID`] and
//! `subject_id = <tag id>`. The lifecycle mirrors `crate::ingest::poller`
//! and `crate::scheduler::runner`:
//!
//! 1. [`crate::scheduler::ledger::claim_or_create`] is the re-entry guard —
//!    subject-keying gives natural per-tag dedup: a regeneration already in
//!    flight (live lease, here or in a peer process) or a failed one inside
//!    its backoff window both come back as [`RegenOutcome::Skipped`], so two
//!    requests for the same tag can never double-run while distinct tags
//!    regenerate concurrently.
//! 2. The mechanism runs (`AtomicCore::generate_wiki`).
//! 3. The row settles terminally with the article id as `result_id`.
//!
//! Unlike system tasks and feed polls, regeneration is *event-triggered*
//! (a manual request or a tag change) — there is no schedule that re-fires
//! it, so a failed run's backed-off retry would otherwise sit in the ledger
//! forever. [`sweep_due_wiki_regens`] closes that gap: the server's
//! scheduler tick scans for runnable `wiki.regenerate` rows (pending past
//! `next_attempt_at`, or crashed with an expired lease) and re-executes
//! them. There is no fast-path cache to maintain — the wiki article row
//! itself is the artifact, and the hot read path (`get_wiki`) never touches
//! `task_runs`.

use crate::error::AtomicCoreError;
use crate::models::{Tag, TaskRunTrigger, WikiArticleWithCitations};
use crate::scheduler::ledger;
use crate::AtomicCore;
use chrono::Utc;

/// `task_runs.task_id` for wiki regenerations; the tag id rides in
/// `subject_id`.
pub const WIKI_REGENERATE_TASK_ID: &str = "wiki.regenerate";

/// Retry budget stamped on freshly inserted rows. Same default contract as
/// the reports, system-task, and feed-poll runners.
pub const MAX_ATTEMPTS: i32 = 3;

/// Terminal outcome of dispatching one wiki regeneration. Mirrors
/// `crate::ingest::poller::PollOutcome` so callers branch on a structured
/// value instead of parsing errors.
#[derive(Debug, Clone)]
pub enum RegenOutcome {
    /// The regeneration ran to completion; the ledger row settled
    /// `succeeded` with the article id as `result_id`.
    Generated(WikiArticleWithCitations),
    /// The regeneration ran and failed; the ledger took the retry-or-abandon
    /// decision and [`sweep_due_wiki_regens`] will pick up the retry once
    /// the backoff window opens.
    Failed { error: String },
    /// `claim_or_create` returned `None`: another worker holds a live lease
    /// on this tag's regeneration, or a failed run is still inside its
    /// backoff window. Callers surface this as "already running" (the HTTP
    /// route maps it to 409) rather than double-running.
    Skipped,
}

/// Regenerate one tag's wiki article through the ledger. `trigger` lands on
/// the `task_runs` row so history can distinguish manual requests from
/// sweeper retries.
///
/// The tag name is resolved from the tags table here — not taken from the
/// caller — so retries always synthesize against the tag's *current* name
/// even if it was renamed after the original request. A nonexistent tag is
/// a caller error (`NotFound`), surfaced before any ledger row is created.
pub async fn run_wiki_regenerate(
    core: &AtomicCore,
    tag_id: &str,
    trigger: TaskRunTrigger,
) -> Result<RegenOutcome, AtomicCoreError> {
    let tag = core
        .storage()
        .get_tag_sync(tag_id)
        .await?
        .ok_or_else(|| AtomicCoreError::NotFound(format!("tag {tag_id}")))?;

    let handle = match ledger::claim_or_create(
        core,
        WIKI_REGENERATE_TASK_ID,
        Some(tag_id),
        trigger,
        MAX_ATTEMPTS,
    )
    .await?
    {
        Some(h) => h,
        None => return Ok(RegenOutcome::Skipped),
    };

    execute(core, &tag, handle).await
}

/// Run the regeneration mechanism under a claimed handle and settle the row.
async fn execute(
    core: &AtomicCore,
    tag: &Tag,
    handle: ledger::RunHandle,
) -> Result<RegenOutcome, AtomicCoreError> {
    match core.generate_wiki(&tag.id, &tag.name).await {
        Ok(article) => {
            let _ = handle.complete(Some(article.article.id.clone())).await?;
            Ok(RegenOutcome::Generated(article))
        }
        Err(e) => {
            let error = e.to_string();
            let _ = handle.fail(error.clone()).await?;
            Ok(RegenOutcome::Failed { error })
        }
    }
}

/// Claim and execute one runnable `wiki.regenerate` row, typically
/// discovered by a ledger scan
/// ([`AtomicCore::list_runnable_task_runs`](crate::AtomicCore::list_runnable_task_runs)).
/// This is the per-row body of [`sweep_due_wiki_regens`], public so hosts
/// that schedule rows individually (rather than sweeping a whole database
/// at once) drive the identical lifecycle:
///
/// - The tag is resolved fresh, so retries synthesize against its *current*
///   name. A tag deleted while the retry was pending settles the row as a
///   moot success (no artifact) and returns [`RegenOutcome::Skipped`] —
///   the work no longer exists.
/// - The conditional claim ([`ledger::claim_existing`]) fences peers: a row
///   that settled or was claimed between the caller's scan and this call
///   returns [`RegenOutcome::Skipped`].
pub async fn run_runnable_wiki_regen(
    core: &AtomicCore,
    run: &crate::models::TaskRun,
) -> Result<RegenOutcome, AtomicCoreError> {
    let Some(tag_id) = run.subject_id.clone() else {
        // Defensive: wiki runs are always subject-keyed. An unkeyed row
        // can't be executed, so leave it for inspection rather than
        // claiming something we can't settle meaningfully.
        tracing::warn!(run_id = %run.id, "[wiki.regenerate] row without subject_id; skipping");
        return Ok(RegenOutcome::Skipped);
    };

    let tag = match core.storage().get_tag_sync(&tag_id).await? {
        Some(tag) => tag,
        None => {
            // The tag was deleted while the retry was pending — the work
            // is moot, not failed. Settle the row (no artifact) so it
            // doesn't stay runnable forever.
            if let Some(handle) = ledger::claim_existing(core, run).await? {
                let _ = handle.complete(None).await;
                tracing::info!(
                    tag_id = %tag_id,
                    "[wiki.regenerate] tag deleted while retry pending; run settled"
                );
            }
            return Ok(RegenOutcome::Skipped);
        }
    };

    let handle = match ledger::claim_existing(core, run).await? {
        Some(h) => h,
        None => return Ok(RegenOutcome::Skipped), // a peer won the row between scan and claim
    };

    execute(core, &tag, handle).await
}

/// One retry sweep over a single database: claim and re-execute every
/// runnable `wiki.regenerate` row. Returns the tag ids whose articles were
/// regenerated; failures settle on their ledger rows (re-backed-off or
/// abandoned) rather than surfacing here, and lost claim races are silent —
/// a row that vanished between our scan and our claim is another worker's
/// business.
///
/// This is the tick body the scheduler loop in `atomic-server::main`
/// drives — extracted here so tests can drive sweeps directly. Overlapping
/// sweeps are safe: the ledger's conditional claim lets exactly one win
/// each row.
pub async fn sweep_due_wiki_regens(core: &AtomicCore) -> Vec<String> {
    let now = Utc::now().to_rfc3339();
    let due = match core
        .storage()
        .list_runnable_task_runs_sync(WIKI_REGENERATE_TASK_ID, &now)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "[wiki.regenerate] list_runnable failed; skipping sweep");
            return vec![];
        }
    };

    let mut regenerated = Vec::new();
    for run in due {
        let tag_id = run.subject_id.clone().unwrap_or_default();
        match run_runnable_wiki_regen(core, &run).await {
            Ok(RegenOutcome::Generated(_)) => regenerated.push(tag_id),
            Ok(RegenOutcome::Failed { error }) => {
                tracing::warn!(
                    tag_id = %tag_id,
                    error = %error,
                    "[wiki.regenerate] retry failed; ledger scheduled another or abandoned"
                );
            }
            Ok(RegenOutcome::Skipped) => {}
            Err(e) => {
                tracing::warn!(tag_id = %tag_id, error = %e, "[wiki.regenerate] dispatch errored");
            }
        }
    }
    regenerated
}
