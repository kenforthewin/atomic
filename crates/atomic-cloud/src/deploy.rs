//! Deploy gating: readiness, the failure-rate policy, and `deploy_runs`
//! history (plan: "Provisioning lifecycle" → "Schema migration on deploy",
//! steps 4-5 + the policy table + "Rollback").
//!
//! The new binary boots in **migrating mode**: liveness (`/health`) is up
//! from the first request, but the public `/ready` endpoint answers 503
//! until the boot fleet migration ([`crate::fleet_migration`]) completes
//! *and* the failure-rate policy admits. One mechanism, one policy, in one
//! place:
//!
//! | Failure rate            | [`DeployStatus`]     | Readiness |
//! |-------------------------|----------------------|-----------|
//! | `x < ready threshold` (1%, incl. 0) | `Ready`  | ready — sub-threshold failures are stragglers: CloudAuth 503s them per request, the reaper retries them |
//! | `ready ≤ x < review` (10%) | `AwaitingReview`  | not ready until an operator runs `deploy advance` |
//! | `x ≥ review`            | `RollbackRequired`   | never ready on this binary |
//! | run > wall-clock limit  | `MigrationTimeout`   | not ready; restart re-runs the fleet (already-migrated tenants no-op) |
//!
//! Every boot attempt persists one `deploy_runs` row (migration 009; the
//! version-stamp short-circuit's `skipped_current` count joined it in
//! migration 022): started/finished timestamps, the run's counts, and the
//! policy verdict — operator history (`atomic-cloud deploy status`) plus
//! the durable home of the awaiting-review acknowledgment.
//!
//! # `deploy advance` (the awaiting-review override)
//!
//! `awaiting_review` means the failure rate is suspicious but plausibly
//! environmental (a subset of tenant databases unreachable during the
//! deploy). The operator inspects `deploy status`, and either redeploys or
//! runs `deploy advance` — which flips every `awaiting_review` run *at the
//! latest target version* to `advanced` **in the control plane**, so every
//! pod holding on that review (each pod boots its own run row) observes the
//! acknowledgment on its next readiness probe and flips ready. The
//! acknowledgment is per-boot-generation by construction: a future deploy
//! inserts fresh rows that start unacknowledged.
//!
//! **`rollback_required` has no override, deliberately.** A ≥10% failure
//! rate is not straggler noise — the migration itself is broken, and every
//! tenant it *did* convert is only safe because migrations are
//! additive-only. Admitting traffic would serve the broken majority a 503
//! wall while the operator "reviews" a migration that needs code changes.
//! The remedy is structural and already safe: redeploy the old binary (it
//! reads the additive schema fine) and fix the migration. An override flag
//! here would just be a footgun pointed at the largest possible blast
//! radius.
//!
//! **Small fleets hit the rollback band on a single tenant — by design.**
//! One permanently broken tenant in a 2-tenant fleet is a 50% failure rate
//! and `rollback_required` with no override; the percentage policy has no
//! special case for small denominators, deliberately — at that scale the
//! operator can and should look at the actual tenant rather than trust a
//! rate. The operator path: inspect `deploy status` for the stored error,
//! fix the broken tenant (restore/repair its database) or delete the
//! account, then redeploy — the fresh gate run finds the fleet healthy.
//! Tuning `--deploy-review-failure-rate` for a small fleet is the knob if
//! the policy itself is wrong for the deployment.
//!
//! # What readiness does NOT gate
//!
//! Readiness is the load balancer's signal, not a request gate: a pod that
//! is "not ready" still serves traffic that reaches it (existing
//! connections, direct pod access). Per-request safety is CloudAuth's
//! straggler gate — a behind-schema tenant 503s regardless of pod
//! readiness. The two layers are complementary, not redundant.

use std::sync::Arc;

use actix_web::HttpResponse;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::control_plane::ControlPlane;
use crate::error::CloudError;
use crate::fleet_migration::{FleetMigrationConfig, FleetMigrator, FleetRunOutcome};
use crate::provision::ClusterConfig;

/// Lifecycle of a deploy run — both the persisted `deploy_runs.deploy_status`
/// vocabulary and (minus `Advanced`, which only exists as an acknowledgment
/// record, and `Abandoned`, which only exists as finalized history) the
/// readiness hold reason. See the module docs for the policy table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeployStatus {
    /// The fleet migration is in flight (or the pod died mid-run).
    Migrating,
    /// Failure rate below the ready threshold: readiness flipped.
    Ready,
    /// Failure rate in the review band: not ready until `deploy advance`.
    AwaitingReview,
    /// Failure rate at/above the rollback threshold: the migration is
    /// broken; redeploy the old binary. No override exists (module docs).
    RollbackRequired,
    /// The run exceeded its wall-clock limit.
    MigrationTimeout,
    /// An operator acknowledged an `AwaitingReview` run; pods holding on it
    /// flip ready on their next probe.
    Advanced,
    /// A `Migrating` row went stale past the run timeout — the pod that
    /// inserted it crashed (or its outcome write never landed) — and a
    /// later boot or `deploy status` finalized it
    /// ([`finalize_abandoned_runs`]). Terminal history: without this, a
    /// dead pod's `migrating` row would shadow `deploy advance` forever.
    Abandoned,
}

impl DeployStatus {
    /// The persisted (and JSON-visible) text form.
    pub fn as_str(self) -> &'static str {
        match self {
            DeployStatus::Migrating => "migrating",
            DeployStatus::Ready => "ready",
            DeployStatus::AwaitingReview => "awaiting_review",
            DeployStatus::RollbackRequired => "rollback_required",
            DeployStatus::MigrationTimeout => "migration_timeout",
            DeployStatus::Advanced => "advanced",
            DeployStatus::Abandoned => "abandoned",
        }
    }

    /// Inverse of [`as_str`](Self::as_str); `None` for unknown text (a row
    /// written by a newer binary — treat as unrecognized, never panic).
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "migrating" => DeployStatus::Migrating,
            "ready" => DeployStatus::Ready,
            "awaiting_review" => DeployStatus::AwaitingReview,
            "rollback_required" => DeployStatus::RollbackRequired,
            "migration_timeout" => DeployStatus::MigrationTimeout,
            "advanced" => DeployStatus::Advanced,
            "abandoned" => DeployStatus::Abandoned,
            _ => return None,
        })
    }
}

impl std::fmt::Display for DeployStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The failure-rate thresholds (plan policy table). Both are fractions of
/// the enumerated fleet; boundaries are inclusive on the *worse* side
/// (`rate == ready_failure_rate` already awaits review, `rate ==
/// review_failure_rate` already requires rollback), exactly as the plan
/// writes them (`1% ≤ x < 10%`, `x ≥ 10%`).
#[derive(Debug, Clone)]
pub struct DeployPolicy {
    /// Below this, the deploy proceeds (default 1%). Sub-threshold failures
    /// are stragglers: per-request 503s + reaper retries.
    pub ready_failure_rate: f64,
    /// Below this (and at/above ready), the deploy awaits operator review
    /// (default 10%). At/above, rollback is required.
    pub review_failure_rate: f64,
}

impl Default for DeployPolicy {
    fn default() -> Self {
        Self {
            ready_failure_rate: 0.01,
            review_failure_rate: 0.10,
        }
    }
}

/// Plan step 5: map a finished run onto the policy table. Timeout wins
/// unconditionally — a run that blew its wall clock proves nothing about
/// the failure rate of the tenants it never reached.
///
/// The rate's denominator is tenants actually *attempted*
/// (`FleetRunOutcome::total`, the lagging set) — rows the version-stamp
/// short-circuit skipped are excluded on both sides, so a deploy where
/// every tenant is already current attempts nothing, rates 0%, and lands
/// `Ready` on control-plane work alone.
pub fn evaluate_policy(outcome: &FleetRunOutcome, policy: &DeployPolicy) -> DeployStatus {
    if outcome.timed_out {
        return DeployStatus::MigrationTimeout;
    }
    let rate = outcome.failure_rate();
    if rate < policy.ready_failure_rate {
        DeployStatus::Ready
    } else if rate < policy.review_failure_rate {
        DeployStatus::AwaitingReview
    } else {
        DeployStatus::RollbackRequired
    }
}

/// One `deploy_runs` row (migration 009; `skipped_current` added in 022).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DeployRun {
    pub id: String,
    pub target_version: i32,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub total: Option<i32>,
    pub migrated: Option<i32>,
    pub failed: Option<i32>,
    /// Active rows the version-stamp short-circuit skipped as already
    /// current ([`FleetRunOutcome::skipped_current`]). This is what makes a
    /// fully-current fleet's `0/0/0` ledger row distinguishable from a run
    /// that saw no active fleet at all — durably, after pods recycle and
    /// the boot log line is gone. `None` on rows finished before migration
    /// 022 (or by an older binary mid-rolling-deploy): unknown, not zero —
    /// a genuine 0 is precisely the suspicious reading.
    pub skipped_current: Option<i32>,
    pub deploy_status: String,
    pub advanced_at: Option<DateTime<Utc>>,
}

/// Insert this boot's run row (`deploy_status = 'migrating'`) and return
/// its id.
pub async fn start_deploy_run(control: &ControlPlane, target: i32) -> Result<String, CloudError> {
    let run_id = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO deploy_runs (id, target_version) VALUES ($1, $2)")
        .bind(&run_id)
        .bind(target)
        .execute(control.pool())
        .await
        .map_err(CloudError::db("recording deploy-run start"))?;
    Ok(run_id)
}

/// Finish a run: counts, timestamp, and the policy verdict.
pub async fn finish_deploy_run(
    control: &ControlPlane,
    run_id: &str,
    outcome: &FleetRunOutcome,
    status: DeployStatus,
) -> Result<(), CloudError> {
    sqlx::query(
        "UPDATE deploy_runs \
         SET finished_at = NOW(), total = $2, migrated = $3, failed = $4, \
             skipped_current = $5, deploy_status = $6 \
         WHERE id = $1",
    )
    .bind(run_id)
    .bind(outcome.total as i32)
    .bind(outcome.migrated as i32)
    .bind(outcome.failed as i32)
    .bind(outcome.skipped_current as i32)
    .bind(status.as_str())
    .execute(control.pool())
    .await
    .map_err(CloudError::db("recording deploy-run outcome"))?;
    Ok(())
}

/// One run's current status — the awaiting-review probe's poll target.
pub async fn deploy_run_status(
    control: &ControlPlane,
    run_id: &str,
) -> Result<Option<DeployStatus>, CloudError> {
    let status: Option<String> =
        sqlx::query_scalar("SELECT deploy_status FROM deploy_runs WHERE id = $1")
            .bind(run_id)
            .fetch_optional(control.pool())
            .await
            .map_err(CloudError::db("reading deploy-run status"))?;
    Ok(status.as_deref().and_then(DeployStatus::parse))
}

/// The most recently started run — `deploy status`'s subject.
pub async fn latest_deploy_run(control: &ControlPlane) -> Result<Option<DeployRun>, CloudError> {
    sqlx::query_as(
        "SELECT id, target_version, started_at, finished_at, total, migrated, failed, \
                skipped_current, deploy_status, advanced_at \
         FROM deploy_runs ORDER BY started_at DESC, id DESC LIMIT 1",
    )
    .fetch_optional(control.pool())
    .await
    .map_err(CloudError::db("reading latest deploy run"))
}

/// How old a `migrating` row must be before [`finalize_abandoned_runs`]
/// treats it as a dead pod's debris, derived from the run's wall-clock
/// limit: one full run budget plus one bookkeeping retry budget (the
/// outcome write retries for up to another `wall_clock_limit` — see
/// [`run_fleet_gate`]), so a live pod's row can never be that old.
pub fn abandoned_run_threshold(config: &FleetMigrationConfig) -> std::time::Duration {
    config.wall_clock_limit * 2
}

/// Finalize stale `migrating` rows as `abandoned` (minor-but-real
/// robustness: a pod killed mid-fleet-run leaves its row `migrating`
/// forever, and the *latest-run* gate in [`advance_deploy`] would report
/// [`NothingToAdvance`] forever — a dead pod must not shadow a live
/// review). Returns how many rows were finalized.
///
/// Run on boot ([`run_fleet_gate`]) and from the operator surface
/// (`deploy status`, `deploy advance`), with `older_than` from
/// [`abandoned_run_threshold`]. The race against a slow-but-alive pod is
/// self-correcting: its eventual `finish_deploy_run` overwrites
/// `abandoned` with the real verdict (the outcome UPDATE is by id,
/// unconditional).
///
/// [`NothingToAdvance`]: AdvanceOutcome::NothingToAdvance
pub async fn finalize_abandoned_runs(
    control: &ControlPlane,
    older_than: std::time::Duration,
) -> Result<u64, CloudError> {
    let stale_secs = older_than.as_secs_f64();
    let finalized = sqlx::query(
        "UPDATE deploy_runs \
         SET deploy_status = $1, finished_at = NOW() \
         WHERE deploy_status = $2 AND started_at < NOW() - make_interval(secs => $3)",
    )
    .bind(DeployStatus::Abandoned.as_str())
    .bind(DeployStatus::Migrating.as_str())
    .bind(stale_secs)
    .execute(control.pool())
    .await
    .map_err(CloudError::db("finalizing abandoned deploy runs"))?
    .rows_affected();
    if finalized > 0 {
        tracing::warn!(
            finalized,
            "deploy runs stuck 'migrating' past the run timeout were \
             finalized as 'abandoned' (dead pods; see `deploy status`)"
        );
    }
    Ok(finalized)
}

/// What `deploy advance` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvanceOutcome {
    /// Every `awaiting_review` run at `target_version` flipped to
    /// `advanced`; pods holding on them go ready on their next probe.
    Advanced { target_version: i32, runs: u64 },
    /// The latest run is `rollback_required`: refused, by design (module
    /// docs — the only remedy is redeploying the old binary).
    RefusedRollbackRequired,
    /// Nothing is awaiting review; the latest run's status is reported.
    NothingToAdvance { status: String },
    /// No deploy run has ever been recorded.
    NoRuns,
}

/// Acknowledge an awaiting-review deploy: flip every `awaiting_review` run
/// at the latest run's target version to `advanced`. Scoping to the target
/// version (rather than one run id) is what makes the acknowledgment
/// fleet-wide — each pod boots its own row, and all of them are reviews of
/// the same binary.
///
/// The gate is the single *most recently started* run, deliberately: a
/// fresh `migrating` row means some pod is still mid-fleet-run at this
/// target, and acknowledging before its verdict lands could advance a
/// review the operator never saw. The operational consequence on multi-pod
/// boots is that `deploy advance` may answer [`NothingToAdvance`] (status
/// `migrating`, or `ready` from the pod that raced past an earlier pod's
/// review) while an earlier run still holds `awaiting_review` — retry once
/// the latest run settles. Conservative by design: the command can refuse
/// and be re-run, but it can never acknowledge a review sight-unseen.
///
/// `abandoned` rows are excluded from the gate subject: they are a dead
/// pod's debris (finalized by [`finalize_abandoned_runs`]), carry no
/// verdict anyone is holding on, and must not shadow a live review the way
/// a stuck `migrating` row otherwise would.
///
/// [`NothingToAdvance`]: AdvanceOutcome::NothingToAdvance
pub async fn advance_deploy(control: &ControlPlane) -> Result<AdvanceOutcome, CloudError> {
    let latest: Option<DeployRun> = sqlx::query_as(
        "SELECT id, target_version, started_at, finished_at, total, migrated, failed, \
                skipped_current, deploy_status, advanced_at \
         FROM deploy_runs WHERE deploy_status <> $1 \
         ORDER BY started_at DESC, id DESC LIMIT 1",
    )
    .bind(DeployStatus::Abandoned.as_str())
    .fetch_optional(control.pool())
    .await
    .map_err(CloudError::db("reading latest actionable deploy run"))?;
    let Some(latest) = latest else {
        return Ok(AdvanceOutcome::NoRuns);
    };
    match DeployStatus::parse(&latest.deploy_status) {
        Some(DeployStatus::RollbackRequired) => return Ok(AdvanceOutcome::RefusedRollbackRequired),
        Some(DeployStatus::AwaitingReview) => {}
        _ => {
            return Ok(AdvanceOutcome::NothingToAdvance {
                status: latest.deploy_status,
            })
        }
    }
    let runs = sqlx::query(
        "UPDATE deploy_runs \
         SET deploy_status = $2, advanced_at = NOW() \
         WHERE target_version = $1 AND deploy_status = $3",
    )
    .bind(latest.target_version)
    .bind(DeployStatus::Advanced.as_str())
    .bind(DeployStatus::AwaitingReview.as_str())
    .execute(control.pool())
    .await
    .map_err(CloudError::db("advancing awaiting-review deploy runs"))?
    .rows_affected();
    Ok(AdvanceOutcome::Advanced {
        target_version: latest.target_version,
        runs,
    })
}

/// This process's readiness, served by the public `/ready` route. Starts in
/// migrating mode; [`run_fleet_gate`] flips it when the boot fleet
/// migration settles. Cheap to clone (shared inner).
#[derive(Clone)]
pub struct Readiness {
    inner: Arc<ReadinessInner>,
}

struct ReadinessInner {
    control: ControlPlane,
    state: tokio::sync::RwLock<ReadyState>,
}

#[derive(Debug, Clone)]
enum ReadyState {
    /// Boot state: the fleet migration hasn't settled.
    Migrating { since: DateTime<Utc> },
    /// The policy admitted (or an operator advanced); serve traffic.
    Ready,
    /// The policy held the pod back. `AwaitingReview` holds re-check their
    /// run row on each probe so a control-plane `deploy advance` flips
    /// every pod without any push channel.
    Holding {
        status: DeployStatus,
        run_id: String,
    },
}

impl Readiness {
    /// A readiness handle in migrating mode — the `serve` boot state.
    pub fn new(control: ControlPlane) -> Self {
        Self {
            inner: Arc::new(ReadinessInner {
                control,
                state: tokio::sync::RwLock::new(ReadyState::Migrating { since: Utc::now() }),
            }),
        }
    }

    /// A readiness handle that is already ready — for compositions that run
    /// no fleet gate (tests, tooling). Production `serve` always boots via
    /// [`new`](Self::new) + [`run_fleet_gate`].
    pub fn ready(control: ControlPlane) -> Self {
        Self {
            inner: Arc::new(ReadinessInner {
                control,
                state: tokio::sync::RwLock::new(ReadyState::Ready),
            }),
        }
    }

    /// Whether `/ready` currently answers 200. (The awaiting-review advance
    /// check happens in [`probe`](Self::probe), not here — this reads the
    /// settled in-process state only.)
    pub async fn is_ready(&self) -> bool {
        matches!(*self.inner.state.read().await, ReadyState::Ready)
    }

    async fn set_ready(&self) {
        *self.inner.state.write().await = ReadyState::Ready;
    }

    async fn hold(&self, status: DeployStatus, run_id: String) {
        *self.inner.state.write().await = ReadyState::Holding { status, run_id };
    }

    /// Answer one readiness probe. 200 `{"status":"ready"}` when serving;
    /// 503 with the holding status otherwise (`migrating`,
    /// `awaiting_review`, `rollback_required`, `migration_timeout`).
    ///
    /// In the `awaiting_review` hold, each probe re-reads the run row: an
    /// operator's `deploy advance` lands in the control plane, and this
    /// poll is how every pod observes it (probes arrive every few seconds —
    /// one point read each). A failed re-read stays not-ready (fail-closed)
    /// with a warning.
    pub async fn probe(&self) -> HttpResponse {
        let state = self.inner.state.read().await.clone();
        match state {
            ReadyState::Ready => HttpResponse::Ok().json(serde_json::json!({
                "status": "ready",
            })),
            ReadyState::Migrating { since } => {
                HttpResponse::ServiceUnavailable().json(serde_json::json!({
                    "status": "migrating",
                    "since": since.to_rfc3339(),
                }))
            }
            ReadyState::Holding { status, run_id } => {
                if status == DeployStatus::AwaitingReview {
                    match deploy_run_status(&self.inner.control, &run_id).await {
                        Ok(Some(DeployStatus::Advanced)) => {
                            tracing::warn!(
                                run_id,
                                "readiness: deploy advance acknowledged in the control \
                                 plane; flipping this pod ready"
                            );
                            self.set_ready().await;
                            return HttpResponse::Ok().json(serde_json::json!({
                                "status": "ready",
                            }));
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(
                                run_id,
                                error = %e,
                                "readiness: advance re-check failed; staying not-ready"
                            );
                        }
                    }
                }
                HttpResponse::ServiceUnavailable().json(serde_json::json!({
                    "status": status.as_str(),
                    "run_id": run_id,
                }))
            }
        }
    }
}

/// How often the deploy gate's bookkeeping writes retry — the same 5s
/// cadence as the fleet runner's enumeration retries; the principle is one
/// and the same (a transient control-plane error at boot must not brick the
/// pod's gate outright).
const BOOKKEEPING_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Retry `op` every [`BOOKKEEPING_RETRY_INTERVAL`] until it succeeds or
/// `deadline` passes — the deploy gate's bookkeeping discipline, mirroring
/// `FleetMigrator::enumerate_until`. `None` means the control plane stayed
/// unwritable for the whole budget.
async fn retry_bookkeeping<T, F, Fut>(
    deadline: tokio::time::Instant,
    context: &str,
    mut op: F,
) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, CloudError>>,
{
    loop {
        match op().await {
            Ok(value) => return Some(value),
            Err(e) => {
                tracing::warn!(error = %e, "deploy gate: {context} failed; retrying");
                let retry_at = tokio::time::Instant::now() + BOOKKEEPING_RETRY_INTERVAL;
                if retry_at >= deadline {
                    return None;
                }
                tokio::time::sleep_until(retry_at).await;
            }
        }
    }
}

/// The boot deploy gate: record the run, drive the [`FleetMigrator`], apply
/// the policy, persist the verdict, and flip (or hold) this process's
/// [`Readiness`] — logging the flip loudly with every policy input. `serve`
/// spawns this concurrently with the HTTP server so liveness is up from the
/// first request (plan step 4).
///
/// Both bookkeeping writes (`start_deploy_run`, `finish_deploy_run`) retry
/// until a wall-clock-limit deadline, exactly like the runner's
/// enumeration: one transient control-plane blip must not leave the pod
/// `Migrating` forever with green liveness (a silently stalled rollout the
/// orchestrator never restarts). When `finish_deploy_run` exhausts its
/// budget anyway, the pod's **in-memory verdict stays authoritative for
/// its own `/ready`** — it flips or holds exactly as if the write had
/// landed; only the control-plane history (and, for `AwaitingReview`, the
/// `deploy advance` target) is missing, which the error log spells out.
pub async fn run_fleet_gate(
    control: ControlPlane,
    cluster: ClusterConfig,
    config: FleetMigrationConfig,
    policy: DeployPolicy,
    readiness: Readiness,
) {
    let target = crate::fleet_migration::tenant_schema_target();

    // Boot housekeeping: dead pods' stuck `migrating` rows must not shadow
    // `deploy advance` (see finalize_abandoned_runs). Best-effort — a
    // failure here never blocks the gate.
    if let Err(e) = finalize_abandoned_runs(&control, abandoned_run_threshold(&config)).await {
        tracing::warn!(error = %e, "deploy gate: finalizing abandoned runs failed; continuing");
    }

    let start_deadline = tokio::time::Instant::now() + config.wall_clock_limit;
    let Some(run_id) = retry_bookkeeping(start_deadline, "recording the deploy run", || {
        start_deploy_run(&control, target)
    })
    .await
    else {
        // The control plane stayed unwritable for a full wall-clock budget
        // of retries: no run row, so no advance target and no history; the
        // pod stays in migrating mode (not ready) until restarted. Loud:
        // this is a control-plane outage at boot, an operator problem.
        tracing::error!(
            "deploy gate: recording the deploy run failed for the whole \
             wall-clock budget; this pod will stay not-ready until restarted"
        );
        return;
    };

    let outcome = FleetMigrator::new(control.clone(), cluster, config.clone())
        .run()
        .await;
    let status = evaluate_policy(&outcome, &policy);

    let finish_deadline = tokio::time::Instant::now() + config.wall_clock_limit;
    if retry_bookkeeping(finish_deadline, "persisting the run outcome", || {
        finish_deploy_run(&control, &run_id, &outcome, status)
    })
    .await
    .is_none()
    {
        // The verdict couldn't be persisted in a full budget of retries.
        // The in-memory verdict below stays authoritative for this pod's
        // own /ready: holding verdicts still hold (locally sound — though
        // `deploy advance` cannot see this run, so an AwaitingReview hold
        // needs a restart or a redeploy to clear), and Ready still flips —
        // refusing traffic over a bookkeeping write would be the worse
        // failure.
        tracing::error!(
            run_id,
            status = status.as_str(),
            "deploy gate: persisting the run outcome failed for the whole \
             wall-clock budget; this pod's in-memory verdict governs its \
             /ready, but the run row still reads 'migrating' — `deploy \
             advance` cannot acknowledge it, and `deploy status` will \
             eventually finalize it as 'abandoned'"
        );
    }

    // The loud flip log the plan asks for: every policy input in one line.
    let rate = outcome.failure_rate();
    match status {
        DeployStatus::Ready => {
            tracing::info!(
                run_id,
                target,
                total = outcome.total,
                skipped_current = outcome.skipped_current,
                migrated = outcome.migrated,
                failed = outcome.failed,
                unattempted = outcome.unattempted(),
                failure_rate = rate,
                elapsed_secs = outcome.elapsed.as_secs(),
                "deploy gate: fleet migration complete; readiness READY"
            );
            readiness.set_ready().await;
        }
        holding => {
            tracing::error!(
                run_id,
                target,
                status = holding.as_str(),
                total = outcome.total,
                skipped_current = outcome.skipped_current,
                migrated = outcome.migrated,
                failed = outcome.failed,
                unattempted = outcome.unattempted(),
                failure_rate = rate,
                timed_out = outcome.timed_out,
                elapsed_secs = outcome.elapsed.as_secs(),
                "deploy gate: fleet migration did NOT pass the policy; \
                 readiness HOLDING (inspect with `atomic-cloud deploy status`)"
            );
            readiness.hold(holding, run_id).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn outcome(total: usize, failed: usize, timed_out: bool) -> FleetRunOutcome {
        FleetRunOutcome {
            target: 22,
            total,
            skipped_current: 0,
            migrated: total - failed,
            failed,
            timed_out,
            elapsed: Duration::from_secs(1),
        }
    }

    /// The production thresholds at their exact boundaries (plan policy
    /// table: `0` and `0 < x < 1%` ready, `1% ≤ x < 10%` review, `x ≥ 10%`
    /// rollback, timeout unconditional).
    #[test]
    fn policy_table_boundaries() {
        let policy = DeployPolicy::default();
        let eval = |total, failed| evaluate_policy(&outcome(total, failed, false), &policy);

        assert_eq!(eval(0, 0), DeployStatus::Ready, "empty fleet");
        assert_eq!(eval(100, 0), DeployStatus::Ready, "0%");
        assert_eq!(eval(1000, 9), DeployStatus::Ready, "0.9% < 1%");
        assert_eq!(eval(100, 1), DeployStatus::AwaitingReview, "exactly 1%");
        assert_eq!(eval(1000, 99), DeployStatus::AwaitingReview, "9.9%");
        assert_eq!(eval(100, 10), DeployStatus::RollbackRequired, "exactly 10%");
        assert_eq!(eval(2, 2), DeployStatus::RollbackRequired, "100%");
        assert_eq!(
            evaluate_policy(&outcome(100, 0, true), &policy),
            DeployStatus::MigrationTimeout,
            "timeout wins even with a clean rate"
        );
    }

    /// The version-stamp short-circuit's gate contract: a run that skipped
    /// the entire fleet on stamps attempted nothing, and the policy must
    /// read that as a clean success — not as suspicious emptiness.
    #[test]
    fn fully_skipped_run_is_a_clean_success() {
        let run = FleetRunOutcome {
            skipped_current: 500,
            ..outcome(0, 0, false)
        };
        assert_eq!(
            evaluate_policy(&run, &DeployPolicy::default()),
            DeployStatus::Ready,
            "100%-skipped run flips readiness"
        );
    }

    #[test]
    fn status_text_roundtrips() {
        for status in [
            DeployStatus::Migrating,
            DeployStatus::Ready,
            DeployStatus::AwaitingReview,
            DeployStatus::RollbackRequired,
            DeployStatus::MigrationTimeout,
            DeployStatus::Advanced,
            DeployStatus::Abandoned,
        ] {
            assert_eq!(DeployStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(DeployStatus::parse("from_the_future"), None);
    }
}
