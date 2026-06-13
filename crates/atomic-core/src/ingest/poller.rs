//! Ledger-dispatched feed polling — durable-task-runs phase 3.
//!
//! The `feeds` table stays the *definition* (url, interval, paused, tags);
//! every poll of a feed becomes a `task_runs` row with
//! `task_id = `[`FEED_POLL_TASK_ID`] and `subject_id = <feed id>`. The
//! lifecycle mirrors `crate::reports::runner` and `crate::scheduler::runner`:
//!
//! 1. [`crate::scheduler::ledger::claim_or_create`] is the re-entry guard —
//!    a live lease (poll in flight, here or in a peer process) or an
//!    unexpired backoff window both come back as [`PollOutcome::Skipped`].
//! 2. The fetch/parse/ingest mechanism runs
//!    (`AtomicCore::fetch_and_ingest_feed`).
//! 3. The row settles terminally and the `feeds` fast-path cache
//!    (`last_polled_at` / `last_error`) is updated.
//!
//! Fast-path semantics mirror `task.{id}.last_run`: `last_polled_at`
//! advances only when a poll *settles* — terminal success, or abandonment
//! after the retry budget — so the hot "which feeds are due" query never
//! touches `task_runs`. A retryable failure leaves `last_polled_at` alone:
//! the feed stays due on every sweep and the pending row's
//! `next_attempt_at` alone decides when the retry fires (exponential
//! backoff instead of waiting out a full `poll_interval`). Advancing the
//! cache on abandonment parks a persistently broken feed until its next
//! regular interval rather than hammering it with back-to-back retry
//! cycles.

use crate::error::AtomicCoreError;
use crate::ingest::{FeedPollResult, IngestionEvent};
use crate::models::TaskRunTrigger;
use crate::scheduler::ledger;
use crate::{AtomicCore, EmbeddingEvent};

/// `task_runs.task_id` for feed polls; the feed id rides in `subject_id`.
pub const FEED_POLL_TASK_ID: &str = "feed_poll";

/// Retry budget stamped on freshly inserted rows. Same default contract as
/// the reports and system-task runners.
pub const MAX_ATTEMPTS: i32 = 3;

/// Terminal outcome of dispatching one feed poll. Mirrors
/// `crate::reports::RunOutcome` so callers branch on a structured value
/// instead of parsing errors.
#[derive(Debug, Clone)]
pub enum PollOutcome {
    /// The poll ran to completion; the ledger row settled `succeeded` and
    /// `last_polled_at` advanced.
    Polled(FeedPollResult),
    /// The poll ran and failed; the ledger took the retry-or-abandon
    /// decision (see the module docs for what each does to the cache).
    Failed { error: String },
    /// `claim_or_create` returned `None`: another worker holds a live
    /// lease on this feed, or a pending row is still inside its backoff
    /// window. Skip — don't retry within the same sweep.
    Skipped,
}

/// Poll one feed through the ledger. `trigger` lands on the `task_runs`
/// row so history can distinguish sweep polls from manual ones.
pub async fn run_feed_poll<F, G>(
    core: &AtomicCore,
    feed_id: &str,
    trigger: TaskRunTrigger,
    on_ingest: F,
    on_embed: G,
) -> Result<PollOutcome, AtomicCoreError>
where
    F: Fn(IngestionEvent) + Send + Sync + Clone + 'static,
    G: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
{
    let handle = match ledger::claim_or_create(
        core,
        FEED_POLL_TASK_ID,
        Some(feed_id),
        trigger,
        MAX_ATTEMPTS,
    )
    .await?
    {
        Some(h) => h,
        None => return Ok(PollOutcome::Skipped),
    };

    match core
        .fetch_and_ingest_feed(feed_id, on_ingest, on_embed)
        .await
    {
        Ok(result) => {
            // Settle the ledger row first, then advance the fast-path —
            // same order as `scheduler::runner`: a cache-write error must
            // not strand a succeeded run in `running` until its lease
            // expires and the whole poll re-executes.
            let _ = handle.complete(None).await?;
            core.storage().mark_feed_polled_sync(feed_id, None).await?;
            Ok(PollOutcome::Polled(result))
        }
        Err(e) => {
            let error = e.to_string();
            // `attempts` was bumped by the claim, so this predicate is
            // exactly the retry-vs-abandon routing `handle.fail` applies.
            let abandoning = handle.run().attempts >= handle.run().max_attempts;
            let _ = handle.fail(error.clone()).await?;
            // The cache write is best-effort diagnostics (mirrors the
            // reports runner): a failed write must not mask the poll error.
            let cache = if abandoning {
                // Retry budget exhausted — advance `last_polled_at` so the
                // feed isn't due again until `poll_interval` elapses.
                core.storage()
                    .mark_feed_polled_sync(feed_id, Some(&error))
                    .await
            } else {
                // Retryable — stamp the error but leave `last_polled_at`
                // alone so the feed stays due and the pending row's backoff
                // window drives the retry.
                core.storage().set_feed_error_sync(feed_id, &error).await
            };
            if let Err(cache_err) = cache {
                tracing::warn!(
                    feed_id = %feed_id,
                    error = %cache_err,
                    "[feed_poll] fast-path cache update failed"
                );
            }
            Ok(PollOutcome::Failed { error })
        }
    }
}

/// One poll sweep over a single database: claim and run every due feed.
///
/// This is the tick body the 60s loop in `atomic-server::main` drives —
/// extracted here so tests can drive sweeps directly. Returns the results
/// of polls that ran to completion; failures settle on their ledger rows
/// (and in `feeds.last_error`) rather than surfacing here, and skips are
/// silent — a live lease or backoff window is another worker's business.
pub async fn poll_due_feeds<F, G>(
    core: &AtomicCore,
    on_ingest: F,
    on_embed: G,
) -> Vec<FeedPollResult>
where
    F: Fn(IngestionEvent) + Send + Sync + Clone + 'static,
    G: Fn(EmbeddingEvent) + Send + Sync + Clone + 'static,
{
    let due_feed_ids: Vec<String> = match core.storage().get_due_feeds_sync().await {
        Ok(feeds) => feeds.into_iter().map(|f| f.id).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "[feed_poll] get_due_feeds failed; skipping sweep");
            return vec![];
        }
    };

    let mut results = Vec::new();
    for feed_id in due_feed_ids {
        match run_feed_poll(
            core,
            &feed_id,
            TaskRunTrigger::Schedule,
            on_ingest.clone(),
            on_embed.clone(),
        )
        .await
        {
            Ok(PollOutcome::Polled(r)) => results.push(r),
            Ok(PollOutcome::Failed { error }) => {
                tracing::warn!(
                    feed_id = %feed_id,
                    error = %error,
                    "[feed_poll] poll failed; ledger scheduled retry or abandoned"
                );
            }
            Ok(PollOutcome::Skipped) => {}
            Err(e) => {
                tracing::warn!(
                    feed_id = %feed_id,
                    error = %e,
                    "[feed_poll] dispatch errored"
                );
            }
        }
    }
    results
}
