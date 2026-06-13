//! Durable execution ledger built on top of [`crate::storage::traits::TaskRunStore`].
//!
//! Wraps the conditional-update storage primitives into the lifecycle the
//! rest of the codebase will use:
//!
//! 1. Insert a `pending` row (or find an existing runnable one) — see
//!    [`claim_or_create`].
//! 2. Win the conditional claim, receiving a [`RunHandle`] that owns a tokio
//!    task heartbeating the lease every [`HEARTBEAT_INTERVAL`].
//! 3. Call [`RunHandle::complete`] on success, [`RunHandle::fail`] on
//!    failure. `fail` consults the host's installed
//!    [`FailureDispositionPolicy`]: environmental failures (provider
//!    limits/outages) route to [`RunHandle::defer_until`], which re-arms the
//!    row WITHOUT consuming retry budget — the lease-reclaim precedent
//!    extended to failures the executor can classify. Dropping the handle
//!    without settling leaves the row in `running`; the next scheduler tick
//!    reclaims it after the lease expires (this is the correct
//!    crash-recovery behavior).
//!
//! Phase 1.5 ships this module dormant — reports (phase 2) will be the
//! first caller. The contract is fully exercised by the unit tests below
//! and the integration tests in `crate::lib` so the semantics are nailed
//! down before any production consumer ships against them.
//!
//! See `docs/plans/reports.md` §"Execution ledger — task_runs" for the
//! state machine, backoff math, and crash-recovery semantics this module
//! implements.

use crate::error::AtomicCoreError;
use crate::models::{TaskRun, TaskRunState, TaskRunTrigger};
use crate::AtomicCore;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rand::Rng;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;

/// How long a claim grants a worker before another tick may reclaim the row.
/// Long enough to ride out a slow LLM-with-tools run; short enough that crash
/// recovery is timely.
pub const LEASE_DURATION: Duration = Duration::from_secs(15 * 60);

/// Cadence at which a running task refreshes its lease. Must be strictly
/// less than [`LEASE_DURATION`] with margin for jitter.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Base unit of the exponential backoff between retries.
const BACKOFF_BASE: Duration = Duration::from_secs(60);

/// Hard cap on backoff so a tight cron (e.g. every 15 minutes) doesn't
/// overshoot the next scheduled fire after a few failures.
const BACKOFF_CAP: Duration = Duration::from_secs(60 * 60);

/// How a failed execution should settle its ledger row.
///
/// The retry budget (`attempts` / `max_attempts`) exists to bound *logic*
/// failures — a task that keeps failing on its own inputs should stop
/// burning resources. **Environmental** failures (a provider rate limit, an
/// exhausted credit allowance, a revoked API key) are different in kind: no
/// retry succeeds until the environment changes, and counting them against
/// the budget terminally abandons work that would succeed the moment it
/// does. This is the same principle the lease-reclaim path already encodes —
/// `reclaim_expired_task_run` deliberately does NOT bump `attempts`, because
/// a process crash isn't a logic failure. Deferral extends that precedent to
/// failures the executor can recognize as environmental.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureDisposition {
    /// A logic failure: consume an attempt — retry with backoff while budget
    /// remains, abandon when it runs out ([`RunHandle::fail`]'s historical
    /// behavior, and the default with no policy installed).
    Fail,
    /// An environmental failure: release the lease, re-arm the row at the
    /// given time, and leave the retry budget untouched
    /// ([`RunHandle::defer_until`]).
    DeferUntil(DateTime<Utc>),
}

/// A host-installed classifier mapping a failure message to its
/// [`FailureDisposition`]. Installed via
/// [`AtomicCore::set_failure_disposition_policy`](crate::AtomicCore::set_failure_disposition_policy);
/// consulted by [`RunHandle::fail`] before settling. Hosts that manage
/// provider environments (pausing/resuming work around provider outages)
/// use this to keep environmental failures from consuming retry budget;
/// with no policy installed every failure is [`FailureDisposition::Fail`].
pub type FailureDispositionPolicy = Arc<dyn Fn(&str) -> FailureDisposition + Send + Sync>;

/// Compute the post-failure backoff for `attempts` completed failures.
///
/// `attempts` is the count of failures so far (i.e., `attempt 1 failed`,
/// `attempt 2 failed`, …), so the first retry uses `backoff(1)`.
///
/// `backoff(n) = clamp(base * 2^(n-1) * rand(0.5, 1.5), [base*0.5, cap])`.
/// The jitter band prevents synchronized retry storms when several
/// scheduler instances fail at the same wall-clock minute.
pub fn backoff(attempts: i32) -> Duration {
    let n = attempts.max(1) as u32;
    // Saturating shift so `1u64 << 63` is the worst case for any
    // realistically reachable `attempts`.
    let factor = 1u64.checked_shl(n.saturating_sub(1)).unwrap_or(u64::MAX);
    let base_secs = BACKOFF_BASE.as_secs_f64();
    let jitter: f64 = rand::thread_rng().gen_range(0.5..1.5);
    let secs = (base_secs * factor as f64 * jitter).min(BACKOFF_CAP.as_secs_f64());
    Duration::from_secs_f64(secs.max(0.0))
}

/// Outcome of a claim attempt.
///
/// `Claimed` means the conditional update won and the caller now owns the
/// run for at most [`LEASE_DURATION`]. `LostRace` means another tick or
/// worker won the same row in between our SELECT and our UPDATE — the
/// caller should treat this as "not ours" and move on, not retry.
//
// The size delta between the two variants is real but the enum is built
// once per claim attempt on a cold path; boxing would cost an alloc to
// shave bytes off a stack value that's already a few hundred wide.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum ClaimOutcome {
    Claimed(TaskRun),
    LostRace,
}

/// Build an ISO-8601 timestamp that lexicographically compares correctly
/// for the SQLite TEXT and Postgres TEXT columns we store dates in.
fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339()
}

/// Find a runnable row for `(task_id, subject_id)` or insert a fresh
/// `pending` row, then attempt the conditional claim.
///
/// Returns `Ok(Some(handle))` when this caller won the claim, `Ok(None)`
/// in two cases:
///
/// 1. The task already has work in flight — either a running row with a
///    live lease, or a pending row whose `next_attempt_at` hasn't arrived
///    yet (post-failure backoff window). Inserting a fresh row here would
///    race past it and start a duplicate execution.
/// 2. A competing scheduler tick won the conditional claim on the same row
///    in between our SELECT and our UPDATE.
///
/// Either way, callers should treat `None` as "not my turn" and exit, not
/// retry within the same tick.
///
/// `max_attempts` and `trigger` are stamped on freshly inserted rows; when
/// reclaiming an existing row those fields are left at their stored values.
pub async fn claim_or_create(
    core: &AtomicCore,
    task_id: &str,
    subject_id: Option<&str>,
    trigger: TaskRunTrigger,
    max_attempts: i32,
) -> Result<Option<RunHandle>, AtomicCoreError> {
    let now = Utc::now();
    let lease_until = now + ChronoDuration::from_std(LEASE_DURATION).unwrap();
    let now_str = iso(now);
    let lease_until_str = iso(lease_until);

    let storage = core.storage();
    // Look for any non-terminal row first, so we can distinguish "no row"
    // (insert + claim) from "row exists but not runnable yet" (skip — a
    // live lease or future-backoff is implicitly owned by someone else).
    let active = storage
        .find_active_task_run_sync(task_id, subject_id)
        .await?;

    let target = match active {
        Some(existing) => {
            if !is_runnable_now(&existing, &now_str) {
                return Ok(None);
            }
            existing
        }
        None => {
            // No active row exists — insert a fresh pending one. The
            // insert goes through `try_insert_task_run` which fences on
            // the `idx_task_runs_active_unique` partial index; if
            // another worker raced past our `find_active` and got there
            // first, the insert returns false and we abort this tick.
            // Without that fence, both workers would insert distinct
            // rows and both claim them — same report runs twice.
            let id = uuid::Uuid::now_v7().to_string();
            let row = TaskRun {
                id,
                task_id: task_id.to_string(),
                subject_id: subject_id.map(|s| s.to_string()),
                state: TaskRunState::Pending,
                trigger,
                attempts: 0,
                max_attempts,
                lease_until: None,
                next_attempt_at: now_str.clone(),
                scope: None,
                result_id: None,
                last_error: None,
                started_at: None,
                finished_at: None,
                created_at: now_str.clone(),
                updated_at: now_str.clone(),
            };
            if !storage.try_insert_task_run_sync(&row).await? {
                return Ok(None);
            }
            row
        }
    };

    let outcome = try_claim(core, &target, &now_str, &lease_until_str).await?;
    let claimed = match outcome {
        ClaimOutcome::Claimed(r) => r,
        ClaimOutcome::LostRace => return Ok(None),
    };

    Ok(Some(RunHandle::spawn(
        core.clone(),
        claimed,
        lease_until_str,
    )))
}

/// Attempt to claim a specific known row — typically one discovered by a
/// sweep over [`crate::storage::traits::TaskRunStore::list_runnable_task_runs`]
/// — without the find-or-insert step of [`claim_or_create`].
///
/// Never inserts: `Ok(None)` means the row settled or was claimed by a peer
/// between the caller's scan and this claim, and the caller should move on
/// rather than fall back to creating fresh work — that's `claim_or_create`'s
/// job for new firings. The conditional UPDATEs in storage enforce the
/// pending-state / expired-lease predicates, so a stale snapshot can't
/// steal a row that has since moved on.
pub async fn claim_existing(
    core: &AtomicCore,
    run: &TaskRun,
) -> Result<Option<RunHandle>, AtomicCoreError> {
    let now = Utc::now();
    let lease_until = now + ChronoDuration::from_std(LEASE_DURATION).unwrap();
    let lease_until_str = iso(lease_until);
    match try_claim(core, run, &iso(now), &lease_until_str).await? {
        ClaimOutcome::Claimed(claimed) => Ok(Some(RunHandle::spawn(
            core.clone(),
            claimed,
            lease_until_str,
        ))),
        ClaimOutcome::LostRace => Ok(None),
    }
}

/// Whether a non-terminal row is runnable right now: pending rows need
/// their `next_attempt_at` to be in the past, running rows need their
/// `lease_until` to have expired. RFC3339 UTC strings compare
/// lexicographically — same convention as the storage `WHERE` clauses.
fn is_runnable_now(row: &TaskRun, now_str: &str) -> bool {
    match row.state {
        TaskRunState::Pending => row.next_attempt_at.as_str() <= now_str,
        TaskRunState::Running => row
            .lease_until
            .as_deref()
            .map(|lu| lu < now_str)
            .unwrap_or(false),
        _ => false,
    }
}

/// Apply the conditional UPDATE — pending → running, or running with
/// expired lease → running. Re-reads the row on success so the caller's
/// `TaskRun` reflects the post-claim state (started_at, lease_until,
/// attempts).
async fn try_claim(
    core: &AtomicCore,
    candidate: &TaskRun,
    now_str: &str,
    lease_until_str: &str,
) -> Result<ClaimOutcome, AtomicCoreError> {
    let storage = core.storage();
    let won = match candidate.state {
        TaskRunState::Pending => {
            storage
                .claim_pending_task_run_sync(&candidate.id, now_str, lease_until_str)
                .await?
        }
        TaskRunState::Running => {
            // Crash-recovery path. The storage layer asserts
            // `lease_until < now`, so a still-leased row will return false
            // here without us needing to re-check timestamps.
            storage
                .reclaim_expired_task_run_sync(&candidate.id, now_str, lease_until_str)
                .await?
        }
        // Terminal states aren't returned by find_runnable, but be defensive.
        _ => return Ok(ClaimOutcome::LostRace),
    };
    if !won {
        return Ok(ClaimOutcome::LostRace);
    }
    let refreshed = storage
        .get_task_run_sync(&candidate.id)
        .await?
        .ok_or_else(|| {
            AtomicCoreError::DatabaseOperation(format!(
                "task_run {} vanished after claim",
                candidate.id
            ))
        })?;
    Ok(ClaimOutcome::Claimed(refreshed))
}

/// Owns a claimed run + the background heartbeat task. Drop without
/// completing or failing leaves the row in `running`; the next scheduler
/// tick will reclaim it after [`LEASE_DURATION`] has elapsed since the last
/// successful heartbeat. This is intentional — we want process crashes to
/// self-heal, not corrupt state.
///
/// `current_lease` is shared with the heartbeat task and used to fence
/// every storage write the handle issues. If a peer reclaims our row
/// (because heartbeats failed to land in time), their reclaim sets a new
/// `lease_until` value; our subsequent `complete` / `fail` then fails the
/// `lease_until = expected` predicate and returns `Ok(false)` rather than
/// stomping the peer's run.
pub struct RunHandle {
    core: AtomicCore,
    run: TaskRun,
    current_lease: Arc<Mutex<String>>,
    heartbeat: Option<JoinHandle<()>>,
}

impl RunHandle {
    fn spawn(core: AtomicCore, run: TaskRun, initial_lease: String) -> Self {
        let current_lease = Arc::new(Mutex::new(initial_lease));
        let heartbeat =
            Self::spawn_heartbeat(core.clone(), run.id.clone(), Arc::clone(&current_lease));
        Self {
            core,
            run,
            current_lease,
            heartbeat: Some(heartbeat),
        }
    }

    fn spawn_heartbeat(
        core: AtomicCore,
        run_id: String,
        current_lease: Arc<Mutex<String>>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
            // Skip the immediate first tick — claim() just set lease_until,
            // there's nothing to refresh yet.
            interval.tick().await;
            loop {
                interval.tick().await;
                let next = iso(Utc::now() + ChronoDuration::from_std(LEASE_DURATION).unwrap());
                let prev = {
                    let guard = current_lease.lock().expect("current_lease poisoned");
                    guard.clone()
                };
                match core
                    .storage()
                    .heartbeat_task_run_sync(&run_id, &prev, &next)
                    .await
                {
                    Ok(true) => {
                        // Lease was successfully extended — record the new
                        // value so subsequent writers fence on it.
                        let mut guard = current_lease.lock().expect("current_lease poisoned");
                        *guard = next;
                    }
                    Ok(false) => {
                        // We've been reclaimed (or the row terminated
                        // some other way). Exit cleanly — the parent's
                        // eventual complete/fail will see the same fence
                        // failure and return Ok(false).
                        tracing::warn!(
                            run_id = %run_id,
                            "[ledger] heartbeat: lease lost; exiting heartbeat task"
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(
                            run_id = %run_id,
                            error = %e,
                            "[ledger] heartbeat: storage error; retrying next interval"
                        );
                    }
                }
            }
        })
    }

    /// The claimed run row. Read-only — mutating helpers return fresh rows
    /// rather than editing this one in place so a stale heartbeat doesn't
    /// leave callers thinking they still own a lease they've lost.
    pub fn run(&self) -> &TaskRun {
        &self.run
    }

    /// Terminal success: `running → succeeded`. Aborts the heartbeat task
    /// first so the next interval tick won't race against the terminal
    /// UPDATE. Returns `Ok(false)` when the row was no longer ours to
    /// complete — either reclaimed by a peer (lease mismatch) or already
    /// transitioned by another caller.
    pub async fn complete(mut self, result_id: Option<String>) -> Result<bool, AtomicCoreError> {
        self.stop_heartbeat();
        let now = iso(Utc::now());
        let lease = self.snapshot_lease();
        self.core
            .storage()
            .complete_task_run_sync(&self.run.id, &lease, result_id.as_deref(), &now)
            .await
    }

    /// Settle a failure. First consults the core's installed
    /// [`FailureDispositionPolicy`] (none installed → [`FailureDisposition::Fail`]):
    /// an environmental classification routes to [`Self::defer_until`], so
    /// the row re-arms without consuming retry budget. A logic failure
    /// consumes an attempt — `fail_task_run_retry` while attempts so far is
    /// less than `max_attempts`, otherwise `fail_task_run_abandon`. The
    /// retry's next_attempt_at is computed from [`backoff`]. Same lease
    /// fence as [`Self::complete`].
    pub async fn fail(mut self, error: String) -> Result<bool, AtomicCoreError> {
        if let FailureDisposition::DeferUntil(until) = self.core.failure_disposition(&error) {
            return self.defer_until(until, error).await;
        }
        self.stop_heartbeat();
        let now = Utc::now();
        let now_str = iso(now);
        let lease = self.snapshot_lease();
        let attempts = self.run.attempts;
        let max_attempts = self.run.max_attempts;
        if attempts < max_attempts {
            let delay = backoff(attempts);
            let next_at = now + ChronoDuration::from_std(delay).unwrap();
            self.core
                .storage()
                .fail_task_run_retry_sync(&self.run.id, &lease, &error, &now_str, &iso(next_at))
                .await
        } else {
            self.core
                .storage()
                .fail_task_run_abandon_sync(&self.run.id, &lease, &error, &now_str)
                .await
        }
    }

    /// Defer the run without consuming retry budget (see
    /// [`FailureDisposition`] for when this is the right settlement): the
    /// row returns to `pending` with `next_attempt_at = until`, the lease is
    /// released, and the attempt the claim charged is refunded — exactly the
    /// lease-reclaim precedent, where interruption by the environment never
    /// counts against the budget. `error` is recorded as `last_error` so run
    /// history shows why the row is waiting. Same lease fence as
    /// [`Self::complete`]; `Ok(false)` means the row was no longer ours.
    pub async fn defer_until(
        mut self,
        until: DateTime<Utc>,
        error: String,
    ) -> Result<bool, AtomicCoreError> {
        self.stop_heartbeat();
        let now_str = iso(Utc::now());
        let lease = self.snapshot_lease();
        self.core
            .storage()
            .defer_task_run_sync(&self.run.id, &lease, &error, &now_str, &iso(until))
            .await
    }

    fn snapshot_lease(&self) -> String {
        self.current_lease
            .lock()
            .expect("current_lease poisoned")
            .clone()
    }

    fn stop_heartbeat(&mut self) {
        if let Some(h) = self.heartbeat.take() {
            h.abort();
        }
    }
}

impl Drop for RunHandle {
    fn drop(&mut self) {
        self.stop_heartbeat();
    }
}

// Send + Sync because every field is Send + Sync (AtomicCore is Clone over Arc).
// Explicit assertion so a future field addition trips the compiler if it
// breaks the contract.
#[allow(dead_code)]
fn _assert_send_sync<T: Send + Sync>() {}
#[allow(dead_code)]
fn _assert_run_handle_send_sync() {
    _assert_send_sync::<RunHandle>();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// First retry (1 prior failure) draws from [base*0.5, base*1.5].
    #[test]
    fn backoff_first_retry_within_jitter_band() {
        for _ in 0..200 {
            let d = backoff(1);
            assert!(
                d >= Duration::from_secs(30),
                "backoff(1) lower bound: {d:?}"
            );
            assert!(
                d <= Duration::from_secs(90),
                "backoff(1) upper bound: {d:?}"
            );
        }
    }

    /// Late retries saturate at the configured cap (with jitter applied to
    /// the pre-cap value, so post-cap is exactly the cap).
    #[test]
    fn backoff_capped_for_high_attempt_counts() {
        for n in [10, 20, 50, 1000] {
            let d = backoff(n);
            assert!(d <= BACKOFF_CAP, "backoff({n}) exceeded cap: {d:?}");
            // For very large n the unjittered factor alone is >> cap, so we
            // should always hit the cap exactly.
            if n >= 10 {
                assert_eq!(d, BACKOFF_CAP, "backoff({n}) expected to saturate");
            }
        }
    }

    /// Sanity: the cap is meaningful — without it, attempts=10 would be
    /// over 8 hours. Catches accidental cap regressions.
    #[test]
    fn backoff_cap_is_below_unbounded_growth() {
        let base = BACKOFF_BASE.as_secs_f64();
        let unbounded_attempts_10 = base * (1u64 << 9) as f64; // 2^9 = 512
        assert!(
            unbounded_attempts_10 > BACKOFF_CAP.as_secs_f64() * 4.0,
            "test assumption: unbounded backoff at n=10 should dwarf the cap"
        );
    }

    /// Both Send and Sync at compile time — needed because RunHandle is
    /// moved into tokio::spawn closures by callers.
    #[test]
    fn run_handle_is_send_sync() {
        _assert_run_handle_send_sync();
    }
}
